# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# This file incorporates work covered by the following copyright and
# permission notice:
#
#   Licensed under the Apache License, Version 2.0 (the "License");
#   you may not use this file except in compliance with the License.
#   You may obtain a copy of the License at
#
#        http://www.apache.org/licenses/LICENSE-2.0
#
#   Unless required by applicable law or agreed to in writing, software
#   distributed under the License is distributed on an "AS IS" BASIS,
#   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
#   See the License for the specific language governing permissions and
#   limitations under the License.

"""Server-side input, clipboard, cursor, and gamepad injection for Selkies.

Both transports (WebSockets and WebRTC) feed client messages into one
WebRTCInput handler, so input authority and fallback behavior stay identical
across modes. Injection follows fallback ladders that resolve the newest
mechanism first and degrade rung by rung, with cooldowns that re-probe the top
rung rather than latching into a fallback:

- Wayland keyboard: the compositor seat keymap (`_WaylandKeymapOwner`), then
  the in-process zwp_virtual_keyboard client, then a data-control clipboard
  paste. The Wayland path is subprocess-free by design; never reintroduce
  wtype, wl-copy or similar forks where the in-process pixelflux harness
  exists.
- X11 keyboard: in-process XTEST (`_XTestKeyboard`, with a dynamic
  spare-keycode overlay for unmapped keysyms and an XKB group lock for
  keysyms a later layout group carries), then an xdotool fork.
- Clipboard: native event-driven monitors (XFixes on X11, compositor
  callbacks / data-control on Wayland), with xclip polling as the X11
  fallback.

Under a nested app compositor the seat rung only carries keysyms the base
layout resolves, so the keyboard worker routes character-bearing keysyms the
base lacks onto the virtual-keyboard text batch by classification, not by
failure. That batch and the selection are aimed at the auto-detected app
compositor: the live `wayland-<N>` socket beside the capture compositor's,
never a differently named relay; aiming at the capture compositor instead
silently kills every non-ASCII key.

Input authority is enforced here, shared by both transports, so a modified
client cannot inject input a controller did not grant: a read-only viewer
peer (shared/#player co-op) may send only `VIEWER_ALLOWED_PREFIXES`; a viewer
holding the active mk token while `enable_collab` is on (a read-write
collaborator) may additionally send `VIEWER_COLLAB_EXTRA_PREFIXES` — the
keyboard, mouse and clipboard set, including `co,` because IME commits and
atomic typing arrive that way. `cmd` and every settings-mutating message stay
controller-only. Blur/visibility lifecycle noise (`VIEWER_SILENT_DROP_PREFIXES`)
from a read-only viewer is normal operation and is dropped without a warning.

Blocking X and compositor work runs on worker threads or dedicated event
threads; the asyncio loop only queues, dispatches, and awaits, so input never
stalls behind a slow display server.
"""

import ctypes
import fcntl
import functools
import logging
import select
import struct
import threading
import time
import asyncio
from asyncio import subprocess
import shutil
import socket
import os
import base64
import io
import re
import json
import aiofiles
import msgpack
from PIL import Image
import urllib.parse
import urllib.request
from typing import Any, Callable, Container, Iterable, Optional, Tuple, Union
from .display_utils import (
    pixelflux_x11_cursor,
    unpremultiply_rgba,
    cursor_content_handle,
    layout_extent,
)
from .media_pipeline import RateControlMode
from .settings import settings, WS_MAX_MESSAGE_BYTES
try:
    from pixelflux import VirtualKeyboardUnavailable as PixelfluxVkUnavailable
except Exception:
    class PixelfluxVkUnavailable(RuntimeError):
        """Stand-in when pixelflux predates the typed exception."""
try:
    libxkb = ctypes.CDLL("libxkbcommon.so.0")
    libxkb.xkb_keysym_to_utf8.argtypes = [ctypes.c_uint32, ctypes.c_char_p, ctypes.c_size_t]
    libxkb.xkb_keysym_to_utf8.restype = ctypes.c_int
    libxkb.xkb_context_new.argtypes = [ctypes.c_int]
    libxkb.xkb_context_new.restype = ctypes.c_void_p
    libxkb.xkb_context_unref.argtypes = [ctypes.c_void_p]
    libxkb.xkb_keymap_new_from_string.argtypes = [
        ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int, ctypes.c_int]
    libxkb.xkb_keymap_new_from_string.restype = ctypes.c_void_p
    libxkb.xkb_keymap_unref.argtypes = [ctypes.c_void_p]
    libxkb.xkb_keymap_min_keycode.argtypes = [ctypes.c_void_p]
    libxkb.xkb_keymap_min_keycode.restype = ctypes.c_uint32
    libxkb.xkb_keymap_max_keycode.argtypes = [ctypes.c_void_p]
    libxkb.xkb_keymap_max_keycode.restype = ctypes.c_uint32
    libxkb.xkb_keymap_num_levels_for_key.argtypes = [
        ctypes.c_void_p, ctypes.c_uint32, ctypes.c_uint32]
    libxkb.xkb_keymap_num_levels_for_key.restype = ctypes.c_uint32
    libxkb.xkb_keymap_key_get_syms_by_level.argtypes = [
        ctypes.c_void_p, ctypes.c_uint32, ctypes.c_uint32, ctypes.c_uint32,
        ctypes.POINTER(ctypes.POINTER(ctypes.c_uint32))]
    libxkb.xkb_keymap_key_get_syms_by_level.restype = ctypes.c_int
    libxkb.xkb_utf32_to_keysym.argtypes = [ctypes.c_uint32]
    libxkb.xkb_utf32_to_keysym.restype = ctypes.c_uint32
except Exception:
    libxkb = None

try:
    from . import Xlib
    from .Xlib import display
    from .Xlib import X
    from .Xlib import XK
    from .Xlib.ext import xfixes, xtest
    from .Xlib.protocol import event as xevent
    from .Xlib import error as xlib_error
    from .x11_xkb import open_xkb_link
    X11_LIBS_AVAILABLE = True
except ImportError:
    X11_LIBS_AVAILABLE = False
    Xlib = None
    display = None
    X = None
    XK = None
    xlib_error = None
    xfixes = None
    xtest = None
    xevent = None
    open_xkb_link = None

try:
    from pixelflux import ScreenCapture
except (ImportError, RuntimeError):
    ScreenCapture = None

VIEWER_ALLOWED_PREFIXES = (
    "SETTINGS,",
    "START_VIDEO",
    # A viewer must pause its own feed on tab hide, or a broadcast-pause can
    # never trigger for viewers.
    "STOP_VIDEO",
    "REQUEST_KEYFRAME",
    "js,",
)
VIEWER_COLLAB_EXTRA_PREFIXES = (
    "kd", "ku", "kh", "kr", "m", "m2",
    "co,",
    "cws", "cbs", "cwd", "cbd", "cwe", "cbe", "cw", "cb", "cr",
    "REQUEST_CLIPBOARD",
)
VIEWER_SILENT_DROP_PREFIXES = ("kr", "cr")


class _WaylandKeymapOwner:
    """Keysym policy owner for the Wayland compositor seat.

    Resolves keysyms to keycodes against the compositor's own keymap
    (synthesizing Shift/AltGr for leveled glyphs) and binds unmapped keysyms —
    Unicode codepoints, IME output — to spare overlay keycodes by swapping in a
    rebuilt keymap. Everything is delivered through inject_key /
    set_keymap_string on the one compositor channel, so key ordering holds.
    Raises on failure so the caller can fall back to the next injection rung.

    Overlay keycodes are chosen from the live keymap (`_build_map`), not a
    fixed range: an X11 keycode is a byte, so a bind above 255 reaches Wayland
    apps only and XWayland clients never see it. The sub-256 range is nearly
    full on a pc105 keymap, so unbound keycodes are taken first, then keycodes
    carrying only XF86 vendor keysyms (media/browser keys a streamed session
    does not need); everything else down there is load bearing (modifiers,
    F-keys, punctuation, Print) and never touched. An overflow band past the
    ceiling keeps a layout with no room working for Wayland clients instead
    of failing outright.

    Attributes:
        _map: Keysym to (keycode, level) in the base keymap.
        _overlay: Keysym to overlay keycode.
        _overlay_order: Overlay keysyms in bind order, for round-robin recycling.
        _overlay_codes: Overlay keycode pool in preference order.
        _pressed: Held keysym to (keycode, synthesized modifier keycodes).
        _mod_refs: Synthesized modifier keycode to holder count.
        _down: Every keycode currently injected down; the one live view that
            synth-skip and conflict-lift decisions read (no compositor query).
        _shift_kc, _shift_r_kc, _altgr_kc: Modifier keycodes for level synthesis.
    """

    _SUB256_CEILING = 256
    _OVERFLOW_BASE_KEYCODE = 257
    _OVERFLOW_SLOTS = 64

    def __init__(self, wayland_input: Any, base_keymap_text: str) -> None:
        if libxkb is None:
            raise RuntimeError("libxkbcommon unavailable")
        if not base_keymap_text:
            raise RuntimeError("empty compositor keymap")
        self._input = wayland_input
        self._base_text = base_keymap_text
        self._map = {}
        self._overlay = {}
        self._overlay_order = []
        self._pressed = {}
        self._mod_refs = {}
        self._down = set()
        self._build_map()

    def _build_map(self) -> None:
        """Compile the base keymap and index every keysym plus the overlay pool.

        One walk over the keymap fills the keysym-to-(keycode, level) map,
        collects overlay candidates, and resolves the synth-modifier keycodes.
        """
        ctx = libxkb.xkb_context_new(0)
        if not ctx:
            raise RuntimeError("xkb_context_new failed")
        try:
            # Trailing arguments are TEXT_V1 and NO_FLAGS.
            km = libxkb.xkb_keymap_new_from_string(
                ctx, self._base_text.encode(), 1, 0)
            if not km:
                raise RuntimeError("keymap compile failed")
            try:
                lo = libxkb.xkb_keymap_min_keycode(km)
                hi = libxkb.xkb_keymap_max_keycode(km)
                syms = ctypes.POINTER(ctypes.c_uint32)()
                unbound, shadowable = [], []
                for kc in range(lo, hi + 1):
                    levels = min(4, libxkb.xkb_keymap_num_levels_for_key(km, kc, 0))
                    seen = spare = 0
                    for level in range(levels):
                        n = libxkb.xkb_keymap_key_get_syms_by_level(
                            km, kc, 0, level, ctypes.byref(syms))
                        for i in range(n):
                            sym = syms[i]
                            if not sym:
                                continue
                            seen += 1
                            if self._VENDOR_FIRST <= sym <= self._VENDOR_LAST:
                                spare += 1
                            if sym not in self._map:
                                self._map[sym] = (kc, level)
                    if kc < self._SUB256_CEILING and kc >= max(lo, 9):
                        if not seen:
                            unbound.append(kc)
                        elif seen == spare:
                            shadowable.append(kc)
                self._overlay_codes = unbound + shadowable + list(range(
                    self._OVERFLOW_BASE_KEYCODE,
                    self._OVERFLOW_BASE_KEYCODE + self._OVERFLOW_SLOTS))
            finally:
                libxkb.xkb_keymap_unref(km)
        finally:
            libxkb.xkb_context_unref(ctx)
        # Fallbacks are the conventional evdev+8 keycodes.
        self._shift_kc = self._map.get(0xFFE1, (50, 0))[0]
        self._shift_r_kc = self._map.get(0xFFE2, (62, 0))[0]
        self._altgr_kc = self._map.get(0xFE03, (108, 0))[0]

    def resolves(self, keysym: int) -> bool:
        """Whether the base keymap carries this keysym on some key/level, so it
        can be injected without an overlay bind."""
        return keysym in self._map

    def _held_conflicts(self, required: Container[int]) -> list:
        """Shift/AltGr keycodes currently down that the target level rejects.

        Held (client-pressed or synthesized), they would shift the injected key
        onto a different glyph. A required Shift is satisfied by either side,
        so neither Shift keycode is lifted then.
        """
        shift_wanted = self._shift_kc in required
        out = []
        for kc in (self._shift_kc, self._shift_r_kc, self._altgr_kc):
            if not kc or kc not in self._down or kc in required:
                continue
            if shift_wanted and kc in (self._shift_kc, self._shift_r_kc):
                continue
            out.append(kc)
        return out

    def _inject(self, kc: int, state: int) -> None:
        (self._down.add if state else self._down.discard)(kc)
        self._input.inject_key(kc, state)

    # XF86 vendor keysym block, matched numerically: a name lookup per keysym
    # doubled the cost of building the map.
    _VENDOR_FIRST = 0x10080000
    _VENDOR_LAST = 0x1008FFFF

    def _mods_for_level(self, level: int) -> tuple:
        """The Shift/AltGr keycodes whose held state selects this keymap level."""
        mods = []
        if level & 1:
            mods.append(self._shift_kc)
        if level & 2:
            mods.append(self._altgr_kc)
        return tuple(mods)

    def _overlay_text(self) -> str:
        """Base keymap text with the occupied overlay slots bound at level 0.

        Only slots in use are declared, at the keycodes the pool handed out
        (mostly existing sub-256 codes being shadowed); hex keysym literals
        need no names. The base's own maximum is kept: a pc105 keymap binds a
        couple of hundred keycodes above 255, and lowering the ceiling would
        drop every one of them.
        """
        base = self._base_text
        max_at = base.index("maximum = ")
        max_end = base.index(";", max_at)
        used = sorted(self._overlay.values())
        base_max = int(base[max_at + len("maximum = "):max_end].strip())
        parts = [base[:max_at],
                 f"maximum = {max([base_max] + used)}"]
        rest = base[max_end:]
        kc_end = rest.index("};")
        parts.append(rest[:kc_end])
        for kc in used:
            parts.append(f"\t<UC{kc:03}> = {kc};\n")
        rest = rest[kc_end:]
        sym_at = rest.index("xkb_symbols")
        open_at = rest.index("{", sym_at)
        depth = 0
        close_at = None
        for idx in range(open_at, len(rest)):
            ch = rest[idx]
            if ch == "{":
                depth += 1
            elif ch == "}":
                depth -= 1
                if depth == 0:
                    close_at = idx
                    break
        if close_at is None:
            raise RuntimeError("unbalanced xkb_symbols section")
        parts.append(rest[:close_at])
        for keysym, kc in self._overlay.items():
            parts.append(
                f"\tkey <UC{kc:03}> {{ [ {overlay_bind_keysym(keysym):#x} ] }};\n")
        parts.append(rest[close_at:])
        return "".join(parts)

    def _overlay_bind_many(self, keysyms: Iterable[int]) -> dict:
        """Assign overlay keycodes to a batch of keysyms in ONE keymap swap.

        A swap costs milliseconds on the compositor thread (which also drives
        input and rendering), so binding a burst one at a time would stall it
        proportionally. A full pool recycles the oldest slot not held down:
        rebinding a pressed keycode would make its release report a different
        symbol than its press did. The swap rides the same command channel as
        the key events and never awaits a reply, so it drains before the keys
        that need it while this loop is never blocked on the compositor;
        `set_keymap_overlay` hands over just the binds, the `set_keymap_string`
        fallback re-sends the whole keymap text (a redundant compile far side).

        Returns:
            `{keysym: keycode}` for every requested keysym; a keysym that could
            not be bound (every slot held down) maps to 0.
        """
        held = {kc for kc, _ in self._pressed.values()}
        out = {}
        fresh = False
        for keysym in dict.fromkeys(keysyms):
            kc = self._overlay.get(keysym)
            if kc is None:
                if len(self._overlay) >= len(self._overlay_codes):
                    victim = next(
                        (s for s in self._overlay_order if self._overlay[s] not in held),
                        None,
                    )
                    if victim is None:
                        out[keysym] = 0
                        continue
                    self._overlay_order.remove(victim)
                    kc = self._overlay.pop(victim)
                else:
                    kc = self._overlay_codes[len(self._overlay)]
                self._overlay[keysym] = kc
                self._overlay_order.append(keysym)
                held.add(kc)
                fresh = True
            out[keysym] = kc
        if fresh:
            binds = [(kc, overlay_bind_keysym(sym))
                     for sym, kc in self._overlay.items()]
            splice = getattr(self._input, "set_keymap_overlay", None)
            if splice is not None:
                splice(binds)
            else:
                self._input.set_keymap_string(self._overlay_text())
        return out

    def _overlay_bind(self, keysym: int) -> int:
        return self._overlay_bind_many([keysym])[keysym]

    def _tap(self, kc: int, mods: Iterable[int],
             into: Optional[list] = None) -> None:
        """Momentary press+release with refcounted modifier synthesis; a
        modifier the client itself holds down is left alone.

        With `into`, the events are appended to that list instead of injected, so a
        caller typing a run of characters can hand the compositor one ordered batch —
        an event at a time costs a channel send and a calloop wake each, on the thread
        that also renders."""
        out = [] if into is None else into
        synthed = []
        for m in mods:
            if m in self._down and m not in self._mod_refs:
                continue
            self._mod_refs[m] = self._mod_refs.get(m, 0) + 1
            if self._mod_refs[m] == 1:
                out.append((m, 1))
            synthed.append(m)
        out.append((kc, 1))
        out.append((kc, 0))
        for m in reversed(synthed):
            refs = self._mod_refs.get(m, 0) - 1
            if refs <= 0:
                self._mod_refs.pop(m, None)
                out.append((m, 0))
            else:
                self._mod_refs[m] = refs
        if into is None:
            self._inject_run(out)

    def _inject_run(self, events: list) -> None:
        """Deliver an ordered run of (keycode, state) events, batched when the
        compositor accepts a batch."""
        if not events:
            return
        for kc, state in events:
            (self._down.add if state else self._down.discard)(kc)
        batch = getattr(self._input, "inject_keys", None)
        if batch is not None:
            batch(events)
            return
        for kc, state in events:
            self._input.inject_key(kc, state)

    def type_text(self, text: str, neutralize: bool = False) -> bool:
        """Type text as momentary taps with at most ONE keymap swap.

        Every missing keysym resolves in a single swap (no per-char swap storm).
        Each char prefers its canonical layout keysym (a ru layout types ф on
        its own key) before falling to the overlay.

        Args:
            text: Characters to tap out in order.
            neutralize: Lift conflicting held Shift/AltGr around the whole run
                so the taps land on their resolved levels.

        Returns:
            False, having typed nothing, when a char cannot be bound at all;
            True once the full run is injected.
        """
        keysyms = []
        for ch in text:
            ks = character_to_layout_keysym(ch)
            if ks not in self._map:
                cp = ord(ch)
                ks = cp if 0x20 <= cp <= 0xFF else (0x01000000 | cp)
            keysyms.append(ks)
        missing = [ks for ks in dict.fromkeys(keysyms) if ks not in self._map]
        overlay = self._overlay_bind_many(missing) if missing else {}
        # Resolve everything before touching state: the False path must have
        # typed nothing and charged no modifier refs.
        resolved_keys = []
        for ks in keysyms:
            resolved = self._map.get(ks)
            if resolved is None and overlay.get(ks):
                resolved = (overlay[ks], 0)
            if resolved is None:
                return False
            resolved_keys.append(resolved)
        # Lift conflicts before building the taps so _down reflects the lift and
        # a shifted char inside the run synthesizes its Shift normally.
        lifted = self._held_conflicts(()) if neutralize else []
        for kc in lifted:
            self._inject(kc, 0)
        events = []
        for kc, level in resolved_keys:
            self._tap(kc, self._mods_for_level(level), into=events)
        self._inject_run(events + [(kc, 1) for kc in reversed(lifted)])
        return True

    def press(self, keysym: int, neutralize: bool = False) -> None:
        """Press a keysym, overlay-binding it first when the base layout lacks it.

        Only modifiers not already down are synthesized; a modifier the client
        holds as its own key is neither charged nor released here.

        Args:
            neutralize: Lift a conflicting held Shift/AltGr around the press and
                restore it after: a client layout's Shift pairing rarely matches
                the seat layout's, so the held modifier would move the key onto
                a different glyph. Chords pass False so Ctrl+Shift+X passes
                through untouched.
        """
        held = self._pressed.get(keysym)
        if held is not None:
            # Auto-repeat re-press: the first press already charged the refcounts.
            self._inject(held[0], 1)
            return
        resolved = self._map.get(keysym)
        if resolved is not None:
            kc, level = resolved
            mods = self._mods_for_level(level)
        else:
            kc = self._overlay_bind(keysym)
            if not kc:
                # Pool exhausted (every slot held down); keycode 0 is not a key.
                return
            mods = ()
        lifted = self._held_conflicts(set(mods)) if neutralize else []
        for m in lifted:
            self._inject(m, 0)
        synthed = []
        for m in mods:
            already = (m in self._down
                       or (m == self._shift_kc and self._shift_r_kc in self._down))
            if already and m not in self._mod_refs:
                continue
            self._mod_refs[m] = self._mod_refs.get(m, 0) + 1
            if self._mod_refs[m] == 1:
                self._inject(m, 1)
            synthed.append(m)
        self._pressed[keysym] = (kc, tuple(synthed))
        self._inject(kc, 1)
        for m in reversed(lifted):
            self._inject(m, 1)

    def release(self, keysym: int) -> None:
        """Release a pressed keysym and un-refcount the modifiers its press synthesized."""
        held = self._pressed.pop(keysym, None)
        if held is None:
            return
        kc, mods = held
        self._inject(kc, 0)
        for m in reversed(mods):
            refs = self._mod_refs.get(m, 0) - 1
            if refs <= 0:
                self._mod_refs.pop(m, None)
                self._inject(m, 0)
            else:
                self._mod_refs[m] = refs

    def reset(self) -> None:
        """Release every held key and synthetic modifier."""
        for keysym in list(self._pressed):
            try:
                self.release(keysym)
            except Exception:
                self._pressed.pop(keysym, None)
        for m in list(self._mod_refs):
            try:
                self._inject(m, 0)
            except Exception:
                pass
        self._mod_refs.clear()
        self._down.clear()

    def adopt_held(self, previous: "_WaylandKeymapOwner") -> None:
        """Carry what a previous owner left down across a base-layout change.

        Held keys and synthesized modifiers are physical keycodes the compositor
        still has pressed, valid under any base keymap, so the releases that
        follow the change must reach them: a fresh owner would drop them as
        no-ops and leave the keys held for good. Overlay binds are not carried —
        the compositor rebuilds its keymap from the new base without them.
        """
        self._pressed.update(previous._pressed)
        self._mod_refs.update(previous._mod_refs)
        self._down.update(previous._down)


def x_display_socket(name: str) -> Optional[str]:
    """Unix socket path of a local X display name (":N", ":N.S", "unix:N");
    None for a TCP/remote name."""
    m = re.fullmatch(r"(?:unix)?:(\d+)(?:\.\d+)?", name.strip())
    return f"/tmp/.X11-unix/X{m.group(1)}" if m else None


def x_display_live(name: str) -> bool:
    """True when an X server accepts connections on the local display ``name``
    right now (a stale socket file or an unstarted server answers False)."""
    path = x_display_socket(name)
    if path is None:
        return False
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.settimeout(0.2)
        try:
            s.connect(path)
            return True
        finally:
            s.close()
    except OSError:
        return False


class _X11ClipboardMonitor:
    """Event-driven X11 CLIPBOARD access on a dedicated Display connection.

    XFixes selection-owner events signal changes (no polling, no xclip forks)
    and reads go through ConvertSelection with INCR support for large payloads.
    Every Display call runs on this class's own event thread (python-xlib is
    not thread-safe); callers hand it work via a self-pipe. Writes are native
    too: offer() takes CLIPBOARD ownership and the event thread serves
    SelectionRequest (TARGETS + text aliases, or the stored image mime) until
    another app takes over (SelectionClear). xclip remains only as the caller's
    fallback rung.

    The readable targets are the same set, in the same precedence order, as
    the Wayland data-control read and the xclip fallback: a target offered on
    one path must be readable on all of them. A file-manager copy arrives as
    a text/uri-list of file:// URIs rather than image bytes and is resolved
    locally, as the xclip path resolves it. Content written from the browser
    is offered on PRIMARY as well as CLIPBOARD, mirroring the middle-click
    paste the Wayland compositor provides natively.

    The Display is opened with a bounded reply wait: the monitor is (re)built
    from the event loop, sometimes while the server is disrupted — exactly
    when an unbounded connection setup would freeze the loop. A build that
    raises part-way releases its connection, since the caller retries on a
    timer and a connection stranded per attempt would exhaust the server's
    client slots within minutes, after which nothing reaches the display.

    Attributes:
        _d: Dedicated Display connection, used only on the event thread.
        _win: Unmapped transfer window; receives SelectionNotify /
            PropertyNotify and holds the transfer property, never visible.
        _image_targets: (atom, mime) pairs read() walks and offer() maps a
            write mime from, in precedence order.
        _text_targets: (atom, name) text targets in precedence order.
        _changed: Set by every selection-owner change.
        _read_lock: One in-flight conversion or offer at a time.
        _own_data: Payload staged by offer() and served by the event thread.
        _own_mime_atom: Image mime atom of the staged payload (None for text).
        _own_clipboard: Whether CLIPBOARD itself is still ours; the payload
            stays staged while PRIMARY is, but a reader only cares about this.
        _cmd_r, _cmd_w: Self-pipe carrying caller requests to the event thread.
    """

    _READ_TIMEOUT_S = 5.0
    # INCR reads accumulate at most this much (matches the Wayland read cap).
    _READ_MAX_BYTES = 64 * 1024 * 1024
    # Cap on a file-manager (text/uri-list) image read from disk.
    _URI_FILE_MAX_BYTES = 10 * 1024 * 1024
    # Stays under the classic 256 KiB request limit (python-xlib has no
    # BIG-REQUESTS); chunks are appended before the SelectionNotify, so no INCR.
    _WRITE_CHUNK = 240 * 1024

    def __init__(self, display_name: Optional[str] = None) -> None:
        self._d = display.Display(display_name, blocking_timeout=INPUT_X_REPLY_TIMEOUT_S)
        self._cmd_r = self._cmd_w = -1
        try:
            self._build()
        except BaseException:
            self._release_resources()
            raise

    def _release_resources(self) -> None:
        for fd in (self._cmd_r, self._cmd_w):
            if fd >= 0:
                try:
                    os.close(fd)
                except OSError:
                    pass
        self._cmd_r = self._cmd_w = -1
        try:
            self._d.close()
        except Exception:
            pass

    def _build(self) -> None:
        """Create the transfer window, intern atoms, arm XFixes, start the event thread.

        Every atom is interned here, before the event thread starts, so read()
        only compares atom ids and no Display call happens off-thread.
        """
        if not self._d.has_extension('XFIXES'):
            raise RuntimeError("XFixes not available")
        self._d.xfixes_query_version()
        screen = self._d.screen()
        self._win = screen.root.create_window(
            0, 0, 1, 1, 0, screen.root_depth, window_class=X.InputOutput,
            event_mask=X.PropertyChangeMask)
        self._clipboard = self._d.get_atom('CLIPBOARD')
        self._primary = self._d.get_atom('PRIMARY')
        self._prop = self._d.get_atom('SELKIES_CLIP')
        self._incr = self._d.get_atom('INCR')
        self._targets = self._d.get_atom('TARGETS')
        self._image_targets = [(self._d.get_atom(m), m) for m in (
            'image/png', 'image/jpeg', 'image/bmp', 'image/webp', 'image/svg+xml',
            'image/svg')]
        self._text_targets = [(self._d.get_atom(t), t) for t in (
            'UTF8_STRING', 'text/plain;charset=utf-8', 'STRING')]
        self._uri_list_atom = self._d.get_atom('text/uri-list')
        self._d.xfixes_select_selection_input(
            self._win, self._clipboard,
            xfixes.XFixesSetSelectionOwnerNotifyMask
            | xfixes.XFixesSelectionWindowDestroyNotifyMask
            | xfixes.XFixesSelectionClientCloseNotifyMask)
        self._d.flush()
        self._atom_atom = self._d.get_atom('ATOM')
        self._multiple = self._d.get_atom('MULTIPLE')
        self._text_alias_atoms = [a for a, _n in self._text_targets] + [
            self._d.get_atom('TEXT'), self._d.get_atom('text/plain')]
        self._changed = threading.Event()
        self._pending_target = None
        self._reply = None
        self._reply_done = threading.Event()
        self._read_lock = threading.Lock()
        self._own_data = None
        self._own_mime_atom = None
        self._own_is_text = False
        self._pending_own = None
        self._own_done = threading.Event()
        self._own_ok = False
        self._own_clipboard = False
        self._cmd_r, self._cmd_w = os.pipe()
        self._stop = False
        self._thread = threading.Thread(target=self._event_loop, daemon=True,
                                        name="X11ClipboardMonitor")
        self._thread.start()

    def _event_loop(self) -> None:
        """Event-thread main loop: drain X events and run self-pipe commands."""
        xfd = self._d.fileno()
        while not self._stop:
            try:
                r, _, _ = select.select([xfd, self._cmd_r], [], [], 1.0)
            except (OSError, ValueError):
                break
            if self._cmd_r in r:
                os.read(self._cmd_r, 64)
                target = self._pending_target
                if target is not None:
                    self._pending_target = None
                    try:
                        self._win.convert_selection(self._clipboard, target,
                                                    self._prop, X.CurrentTime)
                        self._d.flush()
                    except Exception:
                        self._reply = None
                        self._reply_done.set()
                own = self._pending_own
                if own is not None:
                    self._pending_own = None
                    self._take_ownership(own)
            if xfd in r or self._d.pending_events():
                try:
                    while self._d.pending_events():
                        ev = self._d.next_event()
                        self._dispatch_event(ev)
                except Exception:
                    if not self._stop:
                        time.sleep(0.1)

    def _dispatch_event(self, ev: Any) -> None:
        """Route one X event on the event thread.

        One payload is served on CLIPBOARD and PRIMARY, so a SelectionClear
        drops it only once a foreign owner has taken BOTH: a text-selection
        steal of PRIMARY must not orphan the browser-written CLIPBOARD
        payload, and vice versa.
        """
        if isinstance(ev, xfixes.SelectionNotify):
            self._changed.set()
        elif ev.type == X.SelectionNotify:
            self._collect_selection(ev)
        elif ev.type == X.SelectionRequest:
            self._serve_selection(ev)
        elif ev.type == X.SelectionClear:
            if ev.atom == self._clipboard:
                self._own_clipboard = False
            try:
                owners = (self._d.get_selection_owner(self._clipboard),
                          self._d.get_selection_owner(self._primary))
                if any(getattr(o, 'id', o) == self._win.id for o in owners):
                    return
            except Exception:
                pass
            self._own_data = None
            self._own_mime_atom = None

    def _take_ownership(self, payload: tuple) -> None:
        """On the event thread: stage the payload and claim CLIPBOARD + PRIMARY."""
        data, mime_atom, is_text = payload
        try:
            self._own_data = data
            self._own_mime_atom = mime_atom
            self._own_is_text = is_text
            self._win.set_selection_owner(self._clipboard, X.CurrentTime)
            self._win.set_selection_owner(self._primary, X.CurrentTime)
            self._d.flush()
            owner = self._d.get_selection_owner(self._clipboard)
            self._own_ok = (getattr(owner, 'id', owner) == self._win.id)
        except Exception:
            self._own_ok = False
        self._own_clipboard = self._own_ok
        self._own_done.set()

    def _serve_selection(self, ev: Any) -> None:
        """Answer a SelectionRequest for the payload offer() staged (ICCCM).

        Whatever selection the request names, the answer comes from the one
        staged payload, so CLIPBOARD and PRIMARY are backed alike.
        """
        prop = ev.property if ev.property != X.NONE else ev.target
        granted = X.NONE
        try:
            requestor = ev.requestor
            data = self._own_data
            if data is not None and ev.target == self._targets:
                offered = [self._targets]
                if self._own_is_text:
                    offered += self._text_alias_atoms
                elif self._own_mime_atom is not None:
                    offered.append(self._own_mime_atom)
                requestor.change_property(prop, self._atom_atom, 32, offered)
                granted = prop
            elif data is not None and ev.target != self._multiple and (
                    (self._own_is_text and ev.target in self._text_alias_atoms)
                    or ev.target == self._own_mime_atom):
                requestor.change_property(prop, ev.target, 8,
                                          data[:self._WRITE_CHUNK])
                offset = self._WRITE_CHUNK
                while offset < len(data):
                    requestor.change_property(prop, ev.target, 8,
                                              data[offset:offset + self._WRITE_CHUNK],
                                              mode=X.PropModeAppend)
                    offset += self._WRITE_CHUNK
                granted = prop
        except Exception:
            granted = X.NONE
        try:
            notify = xevent.SelectionNotify(
                time=ev.time, requestor=ev.requestor, selection=ev.selection,
                target=ev.target, property=granted)
            ev.requestor.send_event(notify)
            self._d.flush()
        except Exception:
            pass

    def _prop_bytes(self, prop: Any) -> bytes:
        v = prop.value
        if isinstance(v, str):
            return v.encode('latin-1')
        if isinstance(v, (bytes, bytearray)):
            return bytes(v)
        return bytes(bytearray(v))

    def _collect_selection(self, ev: Any) -> None:
        """On the event thread: fetch the converted property (INCR-aware).

        Under INCR each property delete requests the next chunk and a
        zero-length chunk ends the transfer. Events are awaited with the
        remaining deadline so a stalled owner times the read out instead of
        wedging the event thread inside a blocking next_event(), and the total
        is capped like the Wayland read so a hostile owner cannot balloon
        memory; paste requests keep being served mid-transfer.
        """
        try:
            if ev.property == X.NONE:
                self._reply = None
                self._reply_done.set()
                return
            prop = self._win.get_full_property(self._prop, X.AnyPropertyType)
            self._win.delete_property(self._prop)
            self._d.flush()
            if prop is None:
                self._reply = None
            elif prop.property_type == self._incr:
                chunks = []
                total = 0
                deadline = time.monotonic() + self._READ_TIMEOUT_S
                while time.monotonic() < deadline and total <= self._READ_MAX_BYTES:
                    if not self._d.pending_events():
                        remaining = deadline - time.monotonic()
                        if remaining <= 0:
                            break
                        r, _, _ = select.select([self._d.fileno()], [], [], remaining)
                        if not r or not self._d.pending_events():
                            continue
                    e = self._d.next_event()
                    if (e.type == X.PropertyNotify and e.atom == self._prop
                            and e.state == X.PropertyNewValue):
                        part = self._win.get_full_property(self._prop, X.AnyPropertyType)
                        self._win.delete_property(self._prop)
                        self._d.flush()
                        if part is None or len(part.value) == 0:
                            break
                        piece = self._prop_bytes(part)
                        chunks.append(piece)
                        total += len(piece)
                    elif e.type in (X.SelectionRequest, X.SelectionClear) \
                            or isinstance(e, xfixes.SelectionNotify):
                        self._dispatch_event(e)
                self._reply = (b"".join(chunks), 8)
            elif prop.format == 32:
                self._reply = (list(prop.value), 32)
            else:
                self._reply = (self._prop_bytes(prop), prop.format)
            self._reply_done.set()
        except Exception:
            self._reply = None
            self._reply_done.set()

    def _convert_and_wait(self, target_atom: int) -> Optional[tuple]:
        """Request a selection conversion and wait (bounded) for its reply.

        Returns:
            (value, format) — bytes for format 8, an atom list for format 32 —
            or None on timeout/failure.
        """
        with self._read_lock:
            self._reply = None
            self._reply_done.clear()
            self._pending_target = target_atom
            os.write(self._cmd_w, b"x")
            if not self._reply_done.wait(self._READ_TIMEOUT_S):
                return None
            return self._reply

    def read(self, use_binary: bool) -> tuple:
        """Blocking read (call via executor): (data, mime) like read_clipboard —
        text as str with mime 'text/plain', images as bytes with their mime."""
        reply = self._convert_and_wait(self._targets)
        if not reply or reply[1] != 32:
            # A fresh owner (xclip mid-fork) may not serve requests for a moment
            # after the owner-change event; one short retry covers it.
            time.sleep(0.1)
            reply = self._convert_and_wait(self._targets)
            if not reply or reply[1] != 32:
                return None, None
        offered = set(reply[0])
        if use_binary:
            for atom, mime in self._image_targets:
                if atom in offered:
                    got = self._convert_and_wait(atom)
                    if got and got[0]:
                        return bytes(got[0]), mime
            if self._uri_list_atom in offered:
                got = self._convert_and_wait(self._uri_list_atom)
                if got and got[0]:
                    resolved = self._resolve_uri_list_image(bytes(got[0]))
                    if resolved is not None:
                        return resolved
        for atom, _name in self._text_targets:
            if atom in offered:
                got = self._convert_and_wait(atom)
                if got is not None and got[0] is not None:
                    return bytes(got[0]).decode('utf-8', errors='replace'), 'text/plain'
        return None, None

    def _resolve_uri_list_image(self, data_bytes: bytes) -> Optional[tuple]:
        """Resolve a text/uri-list (file-manager copy) to (image_bytes, mime): the
        first local file:// URI with a known image extension, read bounded. Returns
        None when nothing qualifies."""
        mime_map = {'.png': 'image/png', '.jpg': 'image/jpeg', '.jpeg': 'image/jpeg',
                    '.bmp': 'image/bmp', '.webp': 'image/webp', '.svg': 'image/svg+xml'}
        try:
            text = data_bytes.decode('utf-8', errors='replace')
        except Exception:
            return None
        for line in text.splitlines():
            line = line.strip()
            if not line or line.startswith('#'):
                continue
            try:
                parsed = urllib.parse.urlparse(line)
                if parsed.scheme != 'file':
                    continue
                path = urllib.request.url2pathname(parsed.path)
                if not os.path.isfile(path):
                    continue
                mime = mime_map.get(os.path.splitext(path)[1].lower())
                if not mime:
                    continue
                if 0 < os.path.getsize(path) <= self._URI_FILE_MAX_BYTES:
                    with open(path, 'rb') as f:
                        return f.read(self._URI_FILE_MAX_BYTES), mime
            except (OSError, ValueError):
                # ValueError: urlparse rejects malformed bracketed authorities;
                # one bad line must not kill the whole clipboard read.
                continue
        return None

    def offer(self, data: Union[str, bytes], mime_type: str) -> bool:
        """Blocking (call via executor): take CLIPBOARD ownership and serve `data`
        until another app copies. Returns True when ownership was acquired."""
        if not data:
            return False
        is_text = mime_type == "text/plain"
        data_bytes = data if isinstance(data, bytes) else data.encode('utf-8')
        mime_atom = None
        if not is_text:
            known = dict((m, a) for a, m in self._image_targets)
            mime_atom = known.get(mime_type)
            if mime_atom is None:
                return False
        with self._read_lock:
            self._own_done.clear()
            self._own_ok = False
            self._pending_own = (data_bytes, mime_atom, is_text)
            os.write(self._cmd_w, b"o")
            if not self._own_done.wait(self._READ_TIMEOUT_S):
                return False
            return self._own_ok

    async def wait_change(self, timeout: float) -> bool:
        """Await a selection-owner change (True) or timeout (False), consuming
        it: the outbound monitor loop is the one consumer of the change edge."""
        loop = asyncio.get_running_loop()
        got = await loop.run_in_executor(None, self._changed.wait, timeout)
        if got:
            self._changed.clear()
        return got

    async def peek_change(self, timeout: float) -> bool:
        """Await a selection-owner change without consuming it, for a reader
        that wants the fresh content but must leave the edge to the monitor
        loop (which broadcasts it to every client)."""
        loop = asyncio.get_running_loop()
        return await loop.run_in_executor(None, self._changed.wait, timeout)

    def alive(self) -> bool:
        """False once the event thread has exited (display disruption, XFixes
        error). A dead monitor never reports another change, so the outbound
        monitor loop rebuilds it instead of waiting on it forever."""
        return not self._stop and self._thread.is_alive()

    def owns_selection(self) -> bool:
        """True while CLIPBOARD still belongs to the last offer(), i.e. no X
        application has copied since (a PRIMARY steal alone does not count,
        nor does PRIMARY being ours after an X copy took CLIPBOARD)."""
        return self._own_clipboard

    def close(self) -> None:
        self._stop = True
        try:
            os.write(self._cmd_w, b"q")
        except OSError:
            pass
        # Join before close: the thread must leave select() before the display goes.
        self._thread.join(timeout=2.0)
        self._release_resources()


class _XTestKeyboard:
    """Keyboard controller backed by the bundled python-xlib XTEST extension.

    Injects key events through the already-open self.xdisplay connection; a
    separate second X-display connection whose blocking sync could spin at
    100% CPU inside connect() is deliberately avoided.

    Keysyms the layout lacks (Unicode, exotic symbols) bind on demand to spare
    keycodes past the base layout, so they inject in-process via XTEST instead
    of forking xdotool. The bindings are round-robin recycled; the reverse of
    TigerVNC/x0vncserver's XkbAddKeyKeysym, done through core
    ChangeKeyboardMapping.

    On a multi-group layout (`setxkbmap us,ru`) a keysym of a later group sits
    on the same keycode as its group-1 glyph, and the server translates an
    injected keycode under its current group, so the keysym's group is locked
    around the injection and the previous lock restored once the last key that
    needed it is up. The physical keycode is kept (Cyrillic_ef presses the 'a'
    key, as a Russian keyboard does), so scancode-driven clients see the key
    they expect. Keysyms group 1 carries never switch: the server's own group
    then decides, exactly as for a single-group layout.

    Attributes:
        _xkb: XKB link for group placement and locking; None leaves core-keymap
            resolution, which cannot tell groups apart, as the only path.
        _group_hold: `[group locked before the switch, group locked now,
            keysym -> group it was pressed under]`; None while the server's
            own lock is in force.
        _group_restore: Pending linger timer for the group-lock restore.
        _shift_kc: XK_Shift_L keycode; 0 on an exotic keymap (capitals then
            skip Shift).
        _shift_r_kc: XK_Shift_R; a client-held Shift may be on either keycode.
        _altgr_kc: ISO_Level3_Shift, else Mode_switch, for glyphs bound above
            the Shift level ('@' on an Italian or German keymap).
        _effective_mod_keycodes: Keycodes the modifier map actually binds.
        _synth_mods: Keysym to the modifier keycodes press() synthesized for
            it; release() undoes only these, so a modifier the client itself
            holds is never force-released.
        _spare_keycodes: Overlay pool, discovered lazily; `_spare_set` is the
            same as a frozenset.
        _overlay: Keysym to overlay keycode.
        _overlay_value_kc: Bound value (`overlay_bind_keysym`) to keycode, kept
            in step with `_overlay` so value lookups need no scan.
        _overlay_order: Round-robin recycle order.
        _pressed_kc: Keysym to the keycode injected at press; release replays
            it and never re-resolves (matching neko's XKeyEntryGet: the layout
            may shift mid-keystroke).
        _dirty_spares: Reclaimed keycodes whose first bind needs a settle.
    """

    # Settle after rebinding a recycled keycode: xcb toolkits refetch keymaps
    # asynchronously and could translate the queued press with the old symbol.
    _RECYCLE_SETTLE_S = 0.01
    # A group lock outlives the last key that needed it by this long: one switch
    # per run of keystrokes, and the desktop's layout indicator stays put.
    _GROUP_LINGER_S = 0.5

    def __init__(self, xdisplay: Any) -> None:
        self._d = xdisplay
        self._xkb = open_xkb_link(xdisplay) if open_xkb_link is not None else None
        self._group_hold = None
        self._group_restore = None
        self._shift_kc = xdisplay.keysym_to_keycode(0xffe1)
        self._shift_r_kc = xdisplay.keysym_to_keycode(0xffe2)
        self._altgr_kc = (xdisplay.keysym_to_keycode(0xfe03)
                          or xdisplay.keysym_to_keycode(0xff7e))
        self._effective_mod_keycodes = self._read_effective_modifiers()
        self._synth_mods = {}
        self._spare_keycodes = None
        self._spare_set = frozenset()
        self._overlay = {}
        self._overlay_value_kc = {}
        self._overlay_order = []
        self._pressed_kc = {}
        self._dirty_spares = set()

    def _find_spare_keycodes(self) -> list:
        """Every keycode free to repurpose for the overlay.

        Spare means all levels NoSymbol, or carrying a previous overlay bind —
        every level the SAME Unicode-plane keysym, a shape no real layout
        produces (the server echoes a two-sym bind back expanded across all
        levels). Overlay binds persist on the X server across a handler
        restart, so without reclaiming them each restart would shrink the pool
        until typing beyond the layout starves. Modifier-mapped keycodes are
        never spare: pressing one would toggle its modifier under the typed
        char. The full range is scanned (not a fixed cap): more slots make
        recycling — the only case where a slow app can mistranslate a rebound
        keycode — rare.
        """
        info = self._d.display.info
        lo, hi = info.min_keycode, info.max_keycode
        mapping = self._d.get_keyboard_mapping(lo, hi - lo + 1)
        try:
            mod_keycodes = {kc for row in self._d.get_modifier_mapping()
                            for kc in row if kc}
        except Exception:
            mod_keycodes = set()
        spares = []
        for i, syms in enumerate(mapping):
            kc = lo + i
            if kc in mod_keycodes:
                continue
            bound = {s for s in syms if s}
            if not bound:
                spares.append(kc)
            elif len(bound) == 1:
                sym = next(iter(bound))
                if (sym & 0xFF000000) == 0x01000000:
                    # Clients may translate it by its old value until the
                    # rebind's MappingNotify lands: first bind settles like a recycle.
                    spares.append(kc)
                    self._dirty_spares.add(kc)
        self._spare_set = frozenset(spares)
        return spares

    def _free_spares(self) -> list:
        """Spare keycodes not currently bound, in pool order."""
        if self._spare_keycodes is None:
            self._spare_keycodes = self._find_spare_keycodes()
        used = set(self._overlay.values())
        return [kc for kc in self._spare_keycodes if kc not in used]

    def _layout_keycode(self, keysym: int) -> int:
        """keysym_to_keycode, distrusting hits on spare-pool keycodes: the
        display's cached lookup can name a keycode whose bind belongs to a
        previous handler — or to a DIFFERENT keysym after this pool re-purposed
        it — so on a pool keycode only this shim's own live bind carrying
        exactly this value counts. Only keysym forms an overlay bind can carry
        (Latin-1 high half, Unicode plane) can hit the pool, so everything
        else skips the discovery."""
        kc = self._d.keysym_to_keycode(keysym)
        if not kc:
            return 0
        if (keysym & 0xFF000000) == 0x01000000 or 0xA0 <= keysym <= 0xFF:
            if self._spare_keycodes is None:
                self._spare_keycodes = self._find_spare_keycodes()
            if kc in self._spare_set:
                return self._overlay_value_kc.get(keysym, 0)
        return kc

    def _alloc_overlay_keycode(self, keysym: int) -> tuple:
        """Reserve a spare keycode for keysym and record the binding.

        Recycles the oldest binding when the pool is full. The mapping request
        itself is the caller's (single vs batched).

        Returns:
            (keycode, needs_settle) — needs_settle is True when clients may
            still hold a previous mapping for the keycode (an in-process
            recycle, or a reclaimed spare's first bind).
        """
        free = self._free_spares()
        if free:
            kc = free[0]
            needs_settle = kc in self._dirty_spares
            self._dirty_spares.discard(kc)
        else:
            oldest = self._overlay_order.pop(0)
            kc = self._overlay.pop(oldest)
            self._overlay_value_kc.pop(overlay_bind_keysym(oldest), None)
            needs_settle = True
        self._overlay[keysym] = kc
        self._overlay_value_kc[overlay_bind_keysym(keysym)] = kc
        self._overlay_order.append(keysym)
        return kc, needs_settle

    def _overlay_keycode(self, keysym: int) -> Optional[int]:
        """Bind an unmapped keysym to a spare keycode (recycling the oldest) and
        return it, or None if no spare keycode exists."""
        if keysym in self._overlay:
            return self._overlay[keysym]
        if self._spare_keycodes is None:
            self._spare_keycodes = self._find_spare_keycodes()
        if not self._spare_keycodes:
            return None
        kc, needs_settle = self._alloc_overlay_keycode(keysym)
        # Bound at levels 0 and 1 so an accidental Shift cannot change it.
        bind_value = overlay_bind_keysym(keysym)
        self._d.change_keyboard_mapping(kc, [[bind_value, bind_value]])
        self._d.sync()
        if needs_settle:
            time.sleep(self._RECYCLE_SETTLE_S)
        return kc

    def prebind(self, keysyms: Iterable[int]) -> bool:
        """Overlay-bind every unmapped keysym in as few requests as possible.

        One ChangeKeyboardMapping per contiguous spare-keycode run, one sync —
        so a CJK composition commit broadcasts O(1) MappingNotify events
        instead of one per new char; the longest free runs are taken first to
        keep that count down.

        Returns:
            False (nothing bound) when more new keysyms than slots exist, since
            filling would recycle bindings made earlier in the same batch and
            corrupt the text; the caller then falls back without partial
            typing. True otherwise.
        """
        d = self._d
        missing = []
        for ks in dict.fromkeys(keysyms):
            if ks not in self._overlay and not self._layout_keycode(ks):
                missing.append(ks)
        if not missing:
            return True
        if self._spare_keycodes is None:
            self._spare_keycodes = self._find_spare_keycodes()
        if len(missing) > len(self._spare_keycodes):
            return False
        free = self._free_spares()
        runs = []
        i = 0
        while i < len(free):
            j = i
            while j + 1 < len(free) and free[j + 1] == free[j] + 1:
                j += 1
            runs.append(free[i:j + 1])
            i = j + 1
        runs.sort(key=len, reverse=True)
        picked = []
        for run in runs:
            if len(picked) >= len(missing):
                break
            picked.extend(run[:len(missing) - len(picked)])
        recycled_any = any(kc in self._dirty_spares for kc in picked)
        self._dirty_spares.difference_update(picked)
        while len(picked) < len(missing):
            oldest = self._overlay_order.pop(0)
            picked.append(self._overlay.pop(oldest))
            self._overlay_value_kc.pop(overlay_bind_keysym(oldest), None)
            recycled_any = True
        assigns = []
        for ks, kc in zip(missing, picked):
            self._overlay[ks] = kc
            self._overlay_value_kc[overlay_bind_keysym(ks)] = kc
            self._overlay_order.append(ks)
            assigns.append((kc, ks))
        assigns.sort()
        i = 0
        while i < len(assigns):
            j = i
            while j + 1 < len(assigns) and assigns[j + 1][0] == assigns[j][0] + 1:
                j += 1
            d.change_keyboard_mapping(
                assigns[i][0],
                [[overlay_bind_keysym(ks)] * 2 for _kc, ks in assigns[i:j + 1]])
            i = j + 1
        d.sync()
        if recycled_any:
            time.sleep(self._RECYCLE_SETTLE_S)
        return True

    def bindings_intact(self) -> bool:
        """True when every overlay binding still resolves to its keysym in the
        server's map. Distinguishes our own MappingNotify from a foreign layout
        change by SEMANTICS: servers vary in how many notifies one
        ChangeKeyboardMapping emits and report the full keycode range, so
        neither counting nor range matching works — but a self-bind leaves the
        bindings intact and a foreign change wipes them."""
        if not self._overlay:
            return True
        try:
            for ks, kc in self._overlay.items():
                syms = self._d.get_keyboard_mapping(kc, 1)[0]
                if not len(syms) or syms[0] != overlay_bind_keysym(ks):
                    return False
            return True
        except Exception:
            return False

    def refresh_modifier_keycodes(self) -> None:
        """Re-resolve the synth-modifier keycodes from the (already refreshed)
        cache — a modifier remap moves them without touching the overlay."""
        d = self._d
        self._shift_kc = d.keysym_to_keycode(0xffe1)
        self._shift_r_kc = d.keysym_to_keycode(0xffe2)
        self._altgr_kc = (d.keysym_to_keycode(0xfe03)
                          or d.keysym_to_keycode(0xff7e))
        self._effective_mod_keycodes = self._read_effective_modifiers()

    def _read_effective_modifiers(self) -> Optional[set]:
        """Keycodes the server actually treats as modifiers.

        Holding a key only selects a shifted level when that keycode is bound in
        the modifier map. A keymap can carry the Shift keysym without binding it
        (a bare Xvfb with no keymap is the common case), and injecting Shift there
        types the level-0 glyph instead: every capital arrives lowercase.
        """
        try:
            return {kc for row in self._d.get_modifier_mapping() for kc in row if kc}
        except Exception as e:
            logger_webrtc_input.debug(f"modifier map unreadable ({e}); assuming it binds what it names")
            return None

    def invalidate_mapping(self) -> None:
        """A foreign keymap change (setxkbmap, desktop layout switcher) wiped
        our overlay bindings and may have moved modifier keycodes: drop the
        overlay bookkeeping, rediscover spares lazily and re-resolve the
        modifier keycodes. Held keys are kept: release replays the exact
        press-time keycode."""
        self._overlay.clear()
        self._overlay_value_kc.clear()
        self._overlay_order.clear()
        self._spare_keycodes = None
        self._spare_set = frozenset()
        self._dirty_spares.clear()
        if self._xkb is not None:
            self._xkb.invalidate()
        self.refresh_modifier_keycodes()

    def note_mapping_change(self, first_keycode: int, count: int) -> None:
        """A keyboard MappingNotify arrived for this keycode range: refetch the
        XKB placement on the next lookup unless the range lies inside the spare
        pool, where only overlay binds live and the placement never trusts
        them anyway — so this shim's own binds cost no refetch."""
        if self._xkb is None:
            return
        if self._spare_keycodes is not None and all(
                kc in self._spare_set for kc in range(first_keycode, first_keycode + count)):
            return
        self._xkb.invalidate()

    def keyboard_replaced(self, event: Any) -> Optional[tuple]:
        """`(min_keycode, max_keycode)` when the event announces a replaced
        keyboard on the XKB link, else None."""
        if self._xkb is None:
            return None
        return self._xkb.replaced_keyboard(event)

    def outside_base_group(self, keysym: int) -> bool:
        """Whether this keysym must inject through press()/release() rather
        than a bare keycode: it is down under a group lock of this shim's, or
        the layout carries it only in a group past the first."""
        if keysym in self._pressed_kc and self._group_hold is not None:
            return True
        if self._xkb is None:
            return False
        try:
            placed = self._xkb.locate(keysym)
        except Exception as e:
            logger_webrtc_input.debug(f"XKB placement lookup failed ({e}); core keymap only")
            return False
        return placed is not None and placed[1] != 0

    def layout_carries(self, keysym: int) -> bool:
        """Whether the layout itself (not an overlay bind) carries the keysym."""
        try:
            if self._xkb is not None:
                return self._placement(keysym) is not None
        except Exception as e:
            logger_webrtc_input.debug(f"XKB placement lookup failed ({e}); core keymap only")
        return bool(self._layout_keycode(keysym))

    def _placement(self, keysym: int) -> Optional[tuple]:
        """`(keycode, group, level)` from the XKB map, with the spare-pool
        distrust of _layout_keycode applied; None when XKB is unavailable or
        the keymap lacks the keysym."""
        if self._xkb is None:
            return None
        placed = self._xkb.locate(keysym)
        if placed is None:
            return None
        kc, group, level = placed
        if (keysym & 0xFF000000) == 0x01000000 or 0xA0 <= keysym <= 0xFF:
            if self._spare_keycodes is None:
                self._spare_keycodes = self._find_spare_keycodes()
            if kc in self._spare_set:
                kc = self._overlay_value_kc.get(keysym, 0)
                if not kc:
                    return None
                return kc, 0, 0
        return kc, group, level

    def _resolve(self, keysym: int) -> tuple:
        """Return (keycode, modifier_keycodes, group) to inject this keysym.

        The modifiers are the Shift / AltGr keycodes whose held state selects
        the keymap level the keysym sits at, so a glyph bound above the Shift
        level (e.g. AltGr '@') types correctly instead of falling through to
        its level-0 glyph. The group is the layout group the keycode carries
        the keysym in; None for an overlay keycode, which carries one group
        and so types the same under any lock, and when XKB is unavailable and
        the core keymap's flattened columns are all there is.

        A keysym the layout lacks binds to a spare keycode in-process (no
        xdotool fork); overlay keysyms sit at level 0 and never need
        modifiers. The same bind is used when the level a glyph sits at is
        unreachable because its modifier keycode is not in the modifier map:
        the glyph then carries its own case instead of depending on a
        modifier the server will not act on.

        Raises:
            ValueError: No keycode exists and no spare keycode can be bound;
                the caller falls back to xdotool.
        """
        d = self._d
        group = None
        placed = None
        xkb_answered = self._xkb is not None
        if xkb_answered:
            try:
                placed = self._placement(keysym)
            except Exception as e:
                logger_webrtc_input.debug(f"XKB placement lookup failed ({e}); core keymap only")
                xkb_answered = False
        if placed is not None:
            kc, group, level = placed
        elif xkb_answered:
            kc = 0
        else:
            kc = self._layout_keycode(keysym)
            # Lowest column carrying this glyph: 0 base, 1 Shift, 2 AltGr, 3 Shift+AltGr.
            level = next((lvl for lvl in range(4)
                          if d.keycode_to_keysym(kc, lvl) == keysym), 0)
        if not kc:
            kc = self._overlay_keycode(keysym)
            if not kc:
                raise ValueError("no keycode for keysym %r" % (keysym,))
            return kc, (), None
        mods = []
        if level & 1 and self._shift_kc:
            mods.append(self._shift_kc)
        if level & 2 and self._altgr_kc:
            mods.append(self._altgr_kc)
        if mods and not self._modifiers_engage(mods):
            overlay_kc = self._overlay_keycode(keysym)
            if overlay_kc:
                return overlay_kc, (), None
        return kc, tuple(mods), group

    def _modifiers_engage(self, mods: Iterable[int]) -> bool:
        """Whether holding these keycodes actually selects a shifted level."""
        effective = getattr(self, "_effective_mod_keycodes", None)
        if effective is None:
            return True
        return all(kc in effective for kc in mods)

    def _down_mod_keycodes(self, held_keysyms: Iterable[int]) -> set:
        """Shift/AltGr keycodes currently down: the client-held keysyms the
        handler tracks, plus this shim's own synthesized holds. Tracked state
        only, so no press ever pays a query_keymap server round trip."""
        down = set()
        for mods in self._synth_mods.values():
            down.update(mods)
        for ks in held_keysyms:
            if ks == 0xFFE1:
                down.add(self._shift_kc)
            elif ks == 0xFFE2:
                down.add(self._shift_r_kc)
            elif ks in (0xFE03, 0xFF7E):
                down.add(self._altgr_kc)
        down.discard(0)
        return down

    def _mods_to_lift(self, required: Container[int], down: Container[int]) -> list:
        """Down Shift/AltGr keycodes the target level does not want. A required
        Shift is satisfied by either side, so neither is lifted then."""
        lift = []
        if self._shift_kc not in required:
            lift.extend(kc for kc in (self._shift_kc, self._shift_r_kc)
                        if kc in down)
        if self._altgr_kc and self._altgr_kc not in required and self._altgr_kc in down:
            lift.append(self._altgr_kc)
        return lift

    def press(self, keysym: int, neutralize: bool = False,
              held_keysyms: Iterable[int] = ()) -> None:
        """Press a keysym via XTEST, synthesizing the modifiers its level needs.

        Only modifiers not already down are synthesized (a required Shift held
        on either side counts), and only those are undone by release().

        Args:
            neutralize: Lift a held Shift/AltGr the level does not want around
                the press — it would select a different glyph, or push an
                overlay bind onto its empty AltGr levels; chords pass False so
                Ctrl+Shift+X keeps its held modifiers.
            held_keysyms: Level-selecting modifier keysyms the client itself
                holds, consulted instead of a per-press server query.
        """
        kc, mods, group = self._resolve(keysym)
        self._enter_group(keysym, group)
        down = self._down_mod_keycodes(held_keysyms)
        lifted = self._mods_to_lift(set(mods), down) if neutralize else []
        for m in lifted:
            xtest.fake_input(self._d, Xlib.X.KeyRelease, m)
        synth = [m for m in mods
                 if m not in down
                 and not (m == self._shift_kc and self._shift_r_kc in down)]
        for m in synth:
            xtest.fake_input(self._d, Xlib.X.KeyPress, m)
        if synth:
            self._synth_mods[keysym] = synth
        xtest.fake_input(self._d, Xlib.X.KeyPress, kc)
        self._pressed_kc[keysym] = kc
        for m in reversed(lifted):
            xtest.fake_input(self._d, Xlib.X.KeyPress, m)
        self._d.flush()

    def release(self, keysym: int) -> None:
        """Release a keysym, replaying its press-time keycode.

        Only an untracked press is re-resolved: the layout may have changed
        mid-keystroke, so a re-resolve of a tracked one could miss the key.
        """
        kc = self._pressed_kc.pop(keysym, None)
        if kc is None:
            kc, _, _ = self._resolve(keysym)
        self._leave_group(keysym)
        xtest.fake_input(self._d, Xlib.X.KeyRelease, kc)
        for m in reversed(self._synth_mods.pop(keysym, ())):
            xtest.fake_input(self._d, Xlib.X.KeyRelease, m)
        self._settle_group()
        self._d.flush()

    def _enter_group(self, keysym: int, group: Optional[int]) -> None:
        """Lock the group a press needs, queued ahead of the press itself.

        Without a hold of ours in force, a group-1 keysym injects under the
        server's own lock (a later group locked by the user's desktop switcher
        then decides, as for any single-group layout) and only a keysym of a
        later group reads the locked group — one round trip — and switches
        when it differs. While a hold is in force every key is tracked under
        it, so a group-1 key typed during a Cyrillic run switches back and the
        original lock returns once the last tracked key is up.
        """
        if group is None or self._xkb is None:
            return
        hold = self._group_hold
        if hold is None:
            if group == 0:
                return
            before = self._xkb.locked_group()
            if before == group:
                return
            self._xkb.lock_group(group)
            self._group_hold = [before, group, {keysym: group}]
            return
        self._cancel_group_restore()
        if hold[1] != group:
            self._xkb.lock_group(group)
            hold[1] = group
        hold[2][keysym] = group

    def _leave_group(self, keysym: int) -> None:
        """Take a key out of the group hold ahead of its release, putting its
        press-time group back if a later key moved the lock on, so the release
        carries the keysym the press did."""
        hold = self._group_hold
        if hold is None:
            return
        group = hold[2].pop(keysym, None)
        if group is not None and group != hold[1]:
            self._xkb.lock_group(group)
            hold[1] = group

    def _settle_group(self) -> None:
        """With no tracked key left down, schedule the restore of the lock
        found before the switch — immediately when no event loop is running to
        defer it."""
        hold = self._group_hold
        if hold is None or hold[2]:
            return
        try:
            loop = asyncio.get_running_loop()
        except RuntimeError:
            self._restore_group()
            return
        self._cancel_group_restore()
        self._group_restore = loop.call_later(self._GROUP_LINGER_S, self._restore_group)

    def _cancel_group_restore(self) -> None:
        if self._group_restore is not None:
            self._group_restore.cancel()
            self._group_restore = None

    def _restore_group(self) -> None:
        """The linger elapsed: put the group lock back unless a key of ours
        is still down under it."""
        self._group_restore = None
        hold = self._group_hold
        if hold is None or hold[2]:
            return
        self.release_group_lock()

    def release_group_lock(self) -> None:
        """Put back the group lock found before this shim switched it, now:
        after a keyboard reset, or before the connection closes, no keystroke
        follows that a lingering lock would serve, and a lock left behind
        would have the desktop translate every later key under it."""
        self._cancel_group_restore()
        hold, self._group_hold = self._group_hold, None
        if hold is None or hold[0] == hold[1] or self._xkb is None:
            return
        try:
            self._xkb.lock_group(hold[0])
            self._d.flush()
        except Exception as e:
            logger_webrtc_input.debug(f"group lock restore failed: {e}")


class _XTestMouse:
    """Mouse controller backed by the bundled python-xlib XTEST extension."""

    def __init__(self, xdisplay: Any) -> None:
        self._d = xdisplay

    @property
    def position(self) -> tuple:
        """Current pointer position as (root_x, root_y)."""
        p = self._d.screen().root.query_pointer()
        return (p.root_x, p.root_y)

    @position.setter
    def position(self, xy: tuple) -> None:
        x, y = xy
        xtest.fake_input(self._d, Xlib.X.MotionNotify, detail=False,
                         root=Xlib.X.NONE, x=int(x), y=int(y))
        self._d.flush()

    def scroll(self, dx: int, dy: int) -> None:
        d = self._d
        # X core scroll buttons 4=up, 5=down, 6=left, 7=right; positive dy is up.
        def _clicks(btn, n):
            for _ in range(int(abs(n))):
                xtest.fake_input(d, Xlib.X.ButtonPress, btn)
                xtest.fake_input(d, Xlib.X.ButtonRelease, btn)
        if dy:
            _clicks(4 if dy > 0 else 5, dy)
        if dx:
            _clicks(7 if dx > 0 else 6, dx)
        d.flush()

    def press(self, button: int) -> None:
        xtest.fake_input(self._d, Xlib.X.ButtonPress, int(button))
        self._d.flush()

    def release(self, button: int) -> None:
        xtest.fake_input(self._d, Xlib.X.ButtonRelease, int(button))
        self._d.flush()


logger_webrtc_input = logging.getLogger("webrtc_input")
logger_selkies_gamepad = logging.getLogger("selkies_gamepad")

# Bound on one multi-part clipboard transfer; the declared size and the
# accumulated chunks are both checked so a client cannot balloon memory.
MULTIPART_CLIPBOARD_MAX_SIZE = 64 * 1024 * 1024

# X reply wait on the input connection. Generous: no healthy round trip comes
# near it, so it fires only on an unresponsive server (bounded stall + reconnect).
INPUT_X_REPLY_TIMEOUT_S = 20.0

# Poll interval for the X event consumers while no loop reader is armed on the
# input connection, so cursor and keymap changes still land promptly.
INPUT_X_EVENT_POLL_S = 0.02


def _is_within_directory(directory: str, target: str) -> bool:
    """Return True if `target` is `directory` itself or strictly inside it.

    Compares on path-segment boundaries via os.path.commonpath rather than a
    bare string prefix (which would accept sibling dirs sharing a name prefix).
    Both paths should already be absolute/realpath-resolved by the caller.
    """
    directory = os.path.abspath(directory)
    target = os.path.abspath(target)
    try:
        return os.path.commonpath([directory, target]) == directory
    except ValueError:
        # Paths on different drives or a mix of absolute/relative.
        return False

# Event, button and axis codes from linux/input-event-codes.h.
EV_SYN = 0x00
EV_KEY = 0x01
EV_REL = 0x02
EV_ABS = 0x03
EV_MSC = 0x04
SYN_REPORT = 0

BTN_MOUSE = 0x110
BTN_LEFT = 0x110
BTN_RIGHT = 0x111
BTN_MIDDLE = 0x112
BTN_SIDE = 0x113
BTN_EXTRA = 0x114

# Gamepad buttons: A/B/X/Y alias SOUTH/EAST/NORTH/WEST; C and Z are kept for
# the XBox360 bitmask; TL/TR bumpers, SELECT/START/MODE back/start/guide,
# THUMBL/THUMBR stick clicks.
BTN_A = 0x130
BTN_B = 0x131
BTN_C = 0x132
BTN_X = 0x133
BTN_Y = 0x134
BTN_Z = 0x135
BTN_TL = 0x136
BTN_TR = 0x137
BTN_SELECT = 0x13a
BTN_START = 0x13b
BTN_MODE = 0x13c
BTN_THUMBL = 0x13d
BTN_THUMBR = 0x13e


# Absolute axes; Z and RZ usually carry the triggers.
ABS_X = 0x00
ABS_Y = 0x01
ABS_Z = 0x02
ABS_RX = 0x03
ABS_RY = 0x04
ABS_RZ = 0x05
ABS_HAT0X = 0x10
ABS_HAT0Y = 0x11

# JS event types from linux/joystick.h.
JS_EVENT_BUTTON = 0x01
JS_EVENT_AXIS = 0x02
JS_EVENT_INIT = 0x80

# Layout of the C interposer's js_config_t; must match the struct exactly.
INTERPOSER_MAX_BTNS = 512
INTERPOSER_MAX_AXES = 64
CONTROLLER_NAME_MAX_LEN = 255 
C_INTERPOSER_STRUCT_SIZE = 1360

# Per-client wait to accept one bulk (clipboard) chunk before it is left out
# of the rest of that payload; bounds the data-channel drain and the WebSocket
# frame send alike, so a slow link is tolerated while a dead one cannot wedge.
BULK_DRAIN_TIMEOUT_S = 15.0

# Raw bytes per multipart clipboard message: the data channel's 1 MiB ceiling
# (rtc.get_adjusted_chunk_size), never above the WebSocket frame ceiling, less
# verb-prefix and base64 margin; a multiple of 3 so chunks concatenate as base64.
CLIPBOARD_CHUNK_SIZE = min(((WS_MAX_MESSAGE_BYTES - 4096) * 3) // 4,
                           ((1024 * 1024 - 512) * 3) // 4)

# Mouse back/forward buttons are injected as Alt+Left / Alt+Right.
KEYSYM_ALT_L = 0xFFE9
KEYSYM_LEFT_ARROW = 0xFF51
KEYSYM_RIGHT_ARROW = 0xFF53

try:
    from .server_keysym_map import X11_KEYSYM_MAP
except ImportError:
    logger_webrtc_input = logging.getLogger("webrtc_input_fallback_map_import")
    logger_webrtc_input.warning(
        "server_keysym_map.py not found or X11_KEYSYM_MAP not defined. "
        "Keysym mapping will rely entirely on fallback."
    )
    X11_KEYSYM_MAP = {}

# Cyrillic (ЙЦУКЕН) keysyms mapped to the QWERTY keysym sitting at the same
# physical key, row by row:
#   Й Ц У К Е Н Г Ш Щ З -> q w e r t y u i o p
#   Ф Ы В А П Р О Л Д -> a s d f g h j k l
#   Я Ч С М И Т Ь -> z x c v b n m
CYRILLIC_TO_QWERTY_KEYSYM = {
    0x06CA: 0x0071,
    0x06C3: 0x0077,
    0x06D5: 0x0065,
    0x06CB: 0x0072,
    0x06C5: 0x0074,
    0x06CE: 0x0079,
    0x06C7: 0x0075,
    0x06DB: 0x0069,
    0x06DD: 0x006F,
    0x06DA: 0x0070,
    0x06C6: 0x0061,
    0x06D9: 0x0073,
    0x06D7: 0x0064,
    0x06C1: 0x0066,
    0x06D0: 0x0067,
    0x06D2: 0x0068,
    0x06CF: 0x006A,
    0x06CC: 0x006B,
    0x06C4: 0x006C,
    0x06D1: 0x007A,
    0x06DE: 0x0078,
    0x06D3: 0x0063,
    0x06CD: 0x0076,
    0x06C9: 0x0062,
    0x06D4: 0x006E,
    0x06D8: 0x006D,
}


@functools.lru_cache(maxsize=4096)
def keysym_to_character(keysym: int) -> Optional[str]:
    """The printable character a keysym types, or None for keysyms that are not
    plain text (modifiers, navigation, anything decoding to a control code).
    Latin-1 and Unicode-plane keysyms decode directly; the legacy planes
    (Cyrillic, Arabic, Hebrew, Greek, Thai, ...) resolve through libxkbcommon,
    with python-Xlib's table as the fallback. Cached: a session types the same
    keysyms over and over, and each miss costs a ctypes round trip."""
    char = None
    if (keysym & 0xFF000000) == 0x01000000:
        codepoint = keysym & 0x00FFFFFF
        if 0 <= codepoint <= 0x10FFFF:
            try:
                char = chr(codepoint)
            except ValueError:
                return None
    elif 0x20 <= keysym <= 0xFF:
        char = chr(keysym)
    elif keysym == 0x20AC:
        char = '€'
    else:
        if libxkb is not None:
            try:
                buf = ctypes.create_string_buffer(8)
                if libxkb.xkb_keysym_to_utf8(keysym, buf, 8) > 0:
                    char = buf.value.decode('utf-8')
            except Exception:
                char = None
        if not char and XK is not None:
            try:
                name = XK.keysym_to_string(keysym)
                if name and len(name) == 1:
                    char = name
            except Exception:
                char = None
    if char and (ord(char[0]) < 0x20 or 0x7F <= ord(char[0]) <= 0x9F):
        return None
    return char or None


@functools.lru_cache(maxsize=4096)
def character_to_layout_keysym(char: str) -> int:
    """The canonical keysym for a character (the one a physical keyboard layout
    binds: Cyrillic_ef for ф, not its Unicode-plane alias), so text typed onto a
    seat whose layout carries the script lands on real layout keys instead of
    overlay binds. Falls back to the Latin-1/Unicode-plane encoding."""
    codepoint = ord(char)
    if libxkb is not None:
        try:
            keysym = libxkb.xkb_utf32_to_keysym(codepoint)
            if keysym:
                return keysym
        except Exception:
            pass
    return codepoint if 0x20 <= codepoint <= 0xFF else (0x01000000 | codepoint)


@functools.lru_cache(maxsize=4096)
def overlay_bind_keysym(keysym: int) -> int:
    """The keysym VALUE an overlay slot carries for `keysym`.

    An overlay bind is a key we invent, not one a layout author chose, and the
    receiving toolkits' tables for legacy national/publishing keysyms disagree
    across versions (permille, signifblank and the angle brackets die or
    mistranslate on a legacy bind) — while Latin-1 and Unicode-plane keysyms
    translate algorithmically everywhere. So any keysym that spells one
    character is bound in that universal form; charless keysyms (XF86, F-keys)
    keep their semantic value.
    """
    if (keysym & 0xFF000000) == 0x01000000 or 0x20 <= keysym <= 0xFF:
        return keysym
    char = keysym_to_character(keysym)
    if char is not None and len(char) == 1:
        return 0x01000000 | ord(char)
    return keysym


def text_to_wayland_keysyms(text: str) -> list:
    """One keysym per typeable char for the virtual-keyboard typer, in the same
    universal Latin-1/Unicode-plane forms overlay binds carry. This is the
    policy half of vk typing — pixelflux's type_keysyms_wayland taps whatever
    it is given — so which keysym spells a character is decided (and tweaked)
    here, never in the transport."""
    out = []
    for char in text:
        if char in ('\n', '\r'):
            out.append(0xFF0D)
        elif char == '\t':
            out.append(0xFF09)
        else:
            codepoint = ord(char)
            if 0x20 <= codepoint <= 0xFF:
                out.append(codepoint)
            elif codepoint >= 0x100:
                out.append(0x01000000 | codepoint)
    return out


class JsConfigCtypes(ctypes.Structure):
    """ctypes mirror of the C interposer's js_config_t, used only to verify size."""

    _fields_ = [
        ("name", ctypes.c_char * CONTROLLER_NAME_MAX_LEN),
        ("vendor", ctypes.c_uint16),
        ("product", ctypes.c_uint16),
        ("version", ctypes.c_uint16),
        ("num_btns", ctypes.c_uint16),
        ("num_axes", ctypes.c_uint16),
        ("btn_map", ctypes.c_uint16 * INTERPOSER_MAX_BTNS),
        ("axes_map", ctypes.c_uint8 * INTERPOSER_MAX_AXES),
        ("final_alignment_padding", ctypes.c_uint8 * 6)
    ]

EXPECTED_C_STRUCT_SIZE: int = ctypes.sizeof(JsConfigCtypes)
logging.info(f"Expected C js_config_t size (from ctypes): {EXPECTED_C_STRUCT_SIZE} bytes")


ABS_MIN_VAL = -32767
ABS_MAX_VAL = 32767
# evdev triggers are commonly 0-255 (some drivers use 0-1023 or ABS_MAX_VAL).
ABS_TRIGGER_MIN_VAL = 0
ABS_TRIGGER_MAX_VAL = 255
ABS_HAT_MIN_VAL = -1
ABS_HAT_MAX_VAL = 1

STANDARD_XPAD_CONFIG = {
    "name": "Microsoft X-Box 360 pad",
    "vendor_id": 0x045e,
    "product_id": 0x028e,
    "version": 0x0114,

    # Order defines the internal button indices: 0 A, 1 B, 2 X, 3 Y, 4 LB,
    # 5 RB, 6 Back, 7 Start, 8 Guide, 9 Left Stick Click, 10 Right Stick Click.
    "btn_map": [
        BTN_A,
        BTN_B,
        BTN_X,
        BTN_Y,
        BTN_TL,
        BTN_TR,
        BTN_SELECT,
        BTN_START,
        BTN_MODE,
        BTN_THUMBL,
        BTN_THUMBR,
    ],

    # Order defines the internal axis indices: 0 LS X, 1 LS Y, 2 LT, 3 RS X,
    # 4 RS Y, 5 RT, 6 D-Pad X, 7 D-Pad Y.
    "axes_map": [
        ABS_X,
        ABS_Y,
        ABS_Z,
        ABS_RX,
        ABS_RY,
        ABS_RZ,
        ABS_HAT0X,
        ABS_HAT0Y
    ],

    "mapping": {
        # Client (browser Gamepad API) button -> internal button index: A B X Y
        # LB RB are 0-5, Back 8, Start 9, stick presses 10 and 11, Guide 16.
        "btns": {
            0: 0,
            1: 1,
            2: 2,
            3: 3,
            4: 4,
            5: 5,
            8: 6,
            9: 7,
            10: 9,
            11: 10,
            16: 8,
        },
        # Client axis -> internal axis index: left stick 0/1, right stick 2/3.
        "axes": {
            0: 0,
            1: 1,
            2: 3,
            3: 4,
        },
        # Client buttons 6 (LT) and 7 (RT) drive the trigger axes.
        "client_btns_to_internal_axes": {
            6: 2,
            7: 5,
        },
        # Client D-pad buttons 12-15 (Up, Down, Left, Right) -> (hat axis, direction).
        "dpad_to_hat": {
            12: (7, -1),
            13: (7, 1),
            14: (6, -1),
            15: (6, 1),
        },
        "trigger_internal_abstract_axis_indices": [2, 5],
        "hat_internal_abstract_axis_indices": [6, 7],
    }
}

def get_js_event_packed(ev_type: int, number: int, value: float) -> bytes:
    """Pack a js_event struct.

    struct js_event is `{ __u32 time; __s16 value; __u8 type; __u8 number; }`.
    `type` is __u8, so it packs as 'B' — 'b' (signed) raises struct.error the
    moment the 0x80 JS_EVENT_INIT flag is OR'd into the type (value >= 128).
    The millisecond timestamp is masked to fit in u32.
    """
    ts_ms = int(time.time() * 1000) & 0xFFFFFFFF
    return struct.pack("=IhBB", ts_ms, int(value), ev_type, number)

def get_evdev_events_packed(ev_type: int, ev_code: int, ev_value: float,
                            client_arch_bits: int) -> bytes:
    """Pack an input_event and a SYN_REPORT in the client's architecture.

    `struct input_event { struct timeval time; __u16 type; __u16 code;
    __s32 value; }` with timeval's members `long`, whose width the interposer
    reports as sizeof(unsigned long): 8 bytes on a 64-bit client, 4 on 32-bit.
    """
    
    now = time.time()
    ts_sec = int(now)
    ts_usec = int((now - ts_sec) * 1_000_000)

    if client_arch_bits == 64:
        timeval_fmt = "qq"
    else:
        timeval_fmt = "ll"
    
    event_fmt = f"={timeval_fmt}HHi"

    event_data = struct.pack(event_fmt, ts_sec, ts_usec, ev_type, ev_code, int(ev_value))
    syn_event_data = struct.pack(event_fmt, ts_sec, ts_usec, EV_SYN, SYN_REPORT, 0)
    return event_data + syn_event_data

def normalize_axis_value(client_value: float, is_trigger: bool, is_hat: bool,
                         for_js_event: bool = False) -> int:
    """Normalize a client axis value into the evdev/joydev range.

    Args:
        client_value: -1.0..1.0 for sticks, 0.0..1.0 for triggers, -1/0/1 for
            hats.
        is_trigger: The target axis is a trigger.
        is_hat: The target axis is a D-pad hat.
        for_js_event: Scale hat values to the full axis range (joydev
            semantics) instead of -1/0/1 (evdev semantics).

    Triggers map 0..1 onto the full stick range rather than 0..255: joydev and
    evdev consumers treat them as ordinary analog axes.
    """
    if is_hat:
        hat_val = int(max(ABS_HAT_MIN_VAL, min(ABS_HAT_MAX_VAL, round(client_value))))
        if for_js_event:
            return hat_val * ABS_MAX_VAL
        else:
            return hat_val
    if is_trigger:
        return int(ABS_MIN_VAL + client_value * (ABS_MAX_VAL - ABS_MIN_VAL))
    return int(ABS_MIN_VAL + ((client_value + 1) / 2) * (ABS_MAX_VAL - ABS_MIN_VAL))


class GamepadMapper:
    """Maps client (browser Gamepad API) button/axis indices onto the fixed
    Xbox-pad model in STANDARD_XPAD_CONFIG, producing both joydev and evdev
    event payloads."""

    def __init__(self, config_template: dict, client_input_name: str,
                 client_num_btns: int, client_num_axes: int) -> None:
        self.config = config_template
        self.client_input_name = client_input_name

    def get_mapped_events(self, client_event_idx: int, client_value: float,
                          is_button_event: bool) -> Optional[dict]:
        """Translate one client control change into wire-ready event payloads.

        Args:
            client_event_idx: Client button or axis index.
            client_value: The control's new value (see normalize_axis_value).
            is_button_event: True for a button, False for an axis.

        Returns:
            `{'js_event_data': bytes, 'evdev_event_template': (type, code,
            value)}`, or None when the control has no mapping.
        """
        internal_abstract_idx = -1
        is_trigger_axis = False
        is_hat_axis = False
        target_evdev_type = None
        final_value = 0

        if is_button_event:
            if client_event_idx in self.config["mapping"]["dpad_to_hat"]:
                internal_abstract_idx, hat_direction_value = self.config["mapping"]["dpad_to_hat"][client_event_idx]
                is_hat_axis = True
                target_evdev_type = EV_ABS
                final_value = hat_direction_value * int(client_value)
            elif client_event_idx in self.config["mapping"]["client_btns_to_internal_axes"]:
                internal_abstract_idx = self.config["mapping"]["client_btns_to_internal_axes"][client_event_idx]
                is_trigger_axis = internal_abstract_idx in self.config["mapping"]["trigger_internal_abstract_axis_indices"]
                target_evdev_type = EV_ABS
                final_value = client_value
            else:
                internal_abstract_idx = self.config["mapping"]["btns"].get(client_event_idx)
                target_evdev_type = EV_KEY
                final_value = int(client_value)
        else:
            internal_abstract_idx = self.config["mapping"]["axes"].get(client_event_idx)
            is_trigger_axis = internal_abstract_idx in self.config["mapping"]["trigger_internal_abstract_axis_indices"]
            is_hat_axis = internal_abstract_idx in self.config["mapping"]["hat_internal_abstract_axis_indices"]
            target_evdev_type = EV_ABS
            final_value = client_value

        if internal_abstract_idx is None or internal_abstract_idx < 0:
            return None

        evdev_code = -1
        js_event_value = 0
        evdev_event_value = 0

        if target_evdev_type == EV_KEY:
            if 0 <= internal_abstract_idx < len(self.config["btn_map"]):
                evdev_code = self.config["btn_map"][internal_abstract_idx]
                js_event_value = evdev_event_value = final_value
            else: return None
        elif target_evdev_type == EV_ABS:
            if 0 <= internal_abstract_idx < len(self.config["axes_map"]):
                evdev_code = self.config["axes_map"][internal_abstract_idx]
                js_event_value = normalize_axis_value(final_value, is_trigger_axis, is_hat_axis, for_js_event=True)
                evdev_event_value = normalize_axis_value(final_value, is_trigger_axis, is_hat_axis, for_js_event=False)
            else: return None
        else:
            return None

        if evdev_code != -1:
            js_event_type = JS_EVENT_BUTTON if target_evdev_type == EV_KEY else JS_EVENT_AXIS
            js_event_data = get_js_event_packed(js_event_type, internal_abstract_idx, js_event_value)
            
            evdev_event_template = (target_evdev_type, evdev_code, evdev_event_value)
            
            return {'js_event_data': js_event_data, 'evdev_event_template': evdev_event_template}
        
        return None

UINPUT_PATH = "/dev/uinput"
UINPUT_SYSFS_BASE = "/sys/devices/virtual/input"
UINPUT_MAX_NAME_SIZE = 80
BUS_USB = 0x03

# asm-generic ioctl encoding, which is what every architecture Selkies builds for
# (x86_64, aarch64) uses. The exotic layouts (alpha, mips, ppc, sparc) differ.
_IOC_WRITE = 1
_IOC_READ = 2
_IOC_TYPESHIFT = 8
_IOC_SIZESHIFT = 16
_IOC_DIRSHIFT = 30


def _uinput_ioc(direction: int, number: int, size: int) -> int:
    """Encode a uinput ('U') ioctl request number."""
    return ((direction << _IOC_DIRSHIFT) | (ord("U") << _IOC_TYPESHIFT) |
            number | (size << _IOC_SIZESHIFT))


# struct uinput_setup { struct input_id id; char name[80]; __u32 ff_effects_max; }
UINPUT_SETUP_FMT = "=HHHH80sI"
# struct uinput_abs_setup { __u16 code; struct input_absinfo absinfo; }
UINPUT_ABS_SETUP_FMT = "=H2x6i"
UINPUT_SYSNAME_LEN = 64

UI_DEV_CREATE = _uinput_ioc(0, 1, 0)
UI_DEV_DESTROY = _uinput_ioc(0, 2, 0)
UI_DEV_SETUP = _uinput_ioc(_IOC_WRITE, 3, struct.calcsize(UINPUT_SETUP_FMT))
UI_ABS_SETUP = _uinput_ioc(_IOC_WRITE, 4, struct.calcsize(UINPUT_ABS_SETUP_FMT))
UI_SET_EVBIT = _uinput_ioc(_IOC_WRITE, 100, 4)
UI_SET_KEYBIT = _uinput_ioc(_IOC_WRITE, 101, 4)
UI_SET_ABSBIT = _uinput_ioc(_IOC_WRITE, 103, 4)
UI_GET_SYSNAME = _uinput_ioc(_IOC_READ, 44, UINPUT_SYSNAME_LEN)

# (min, max, fuzz, flat, resolution) per axis, matching the interposer's
# EVIOCGABS answer so an application cannot tell the two backends apart.
UINPUT_ABS_INFO_DEFAULT = (ABS_MIN_VAL, ABS_MAX_VAL, 16, 128, 1)
UINPUT_ABS_INFO = {
    ABS_HAT0X: (ABS_HAT_MIN_VAL, ABS_HAT_MAX_VAL, 0, 0, 0),
    ABS_HAT0Y: (ABS_HAT_MIN_VAL, ABS_HAT_MAX_VAL, 0, 0, 0),
}

LOCAL_ARCH_BITS = 64 if struct.calcsize("P") == 8 else 32


def uinput_writable() -> bool:
    """Whether this process can create kernel input devices."""
    try:
        fd = os.open(UINPUT_PATH, os.O_WRONLY | os.O_NONBLOCK)
    except OSError:
        return False
    os.close(fd)
    return True


def interposer_configured() -> bool:
    """Whether this session preloads the Joystick Interposer into applications,
    which already delivers gamepad events without any kernel device."""
    if os.environ.get("SELKIES_INTERPOSER"):
        return True
    return "selkies_joystick_interposer" in os.environ.get("LD_PRELOAD", "")


def uinput_gamepads_enabled(mode: Optional[str]) -> bool:
    """Resolve the uinput_gamepad setting to a decision, logging why.

    'auto' makes kernel gamepads the fallback for hosts that do not preload the
    interposer: it stays off wherever the interposer is set up, so no
    application ever sees the same pad through both backends at once.
    """
    mode = str(mode or "auto").strip().lower()
    if mode in ("false", "0", "no", "off"):
        return False
    forced = mode in ("true", "1", "yes", "on")
    if not forced and mode != "auto":
        logger_selkies_gamepad.warning(f"Unrecognized uinput_gamepad value '{mode}'; using 'auto'.")
    if not forced and interposer_configured():
        logger_selkies_gamepad.info(
            "Joystick Interposer is configured for this session; kernel gamepads stay off."
        )
        return False
    if not uinput_writable():
        log = logger_selkies_gamepad.error if forced else logger_selkies_gamepad.info
        log(
            f"{UINPUT_PATH} is missing or not writable, so gamepads reach applications only "
            "through the Joystick Interposer. Load the uinput module and grant this user "
            "write access to enable kernel gamepads."
        )
        return False
    return True


class UInputGamepad:
    """One slot's kernel gamepad, created through /dev/uinput.

    Applications discover it as an ordinary controller, so neither the Joystick
    Interposer nor fake-udev is involved. It carries the evdev event stream the
    interposer socket carries, presented as the same Xbox pad.
    """

    def __init__(self, label: str) -> None:
        self.label = label
        self.fd: Optional[int] = None
        self.device_nodes: list = []

    def create(self) -> list:
        """Register the kernel device and return its /dev/input node paths.

        Raises:
            OSError: The uinput setup ioctls failed; the fd is closed first.
        """
        fd = os.open(UINPUT_PATH, os.O_WRONLY | os.O_NONBLOCK)
        try:
            fcntl.ioctl(fd, UI_SET_EVBIT, EV_KEY)
            fcntl.ioctl(fd, UI_SET_EVBIT, EV_ABS)
            for code in STANDARD_XPAD_CONFIG["btn_map"]:
                fcntl.ioctl(fd, UI_SET_KEYBIT, code)
            for code in STANDARD_XPAD_CONFIG["axes_map"]:
                fcntl.ioctl(fd, UI_SET_ABSBIT, code)
                minimum, maximum, fuzz, flat, resolution = UINPUT_ABS_INFO.get(
                    code, UINPUT_ABS_INFO_DEFAULT
                )
                fcntl.ioctl(fd, UI_ABS_SETUP, struct.pack(
                    UINPUT_ABS_SETUP_FMT, code, 0, minimum, maximum, fuzz, flat, resolution
                ))
            fcntl.ioctl(fd, UI_DEV_SETUP, struct.pack(
                UINPUT_SETUP_FMT,
                BUS_USB,
                STANDARD_XPAD_CONFIG["vendor_id"],
                STANDARD_XPAD_CONFIG["product_id"],
                STANDARD_XPAD_CONFIG["version"],
                STANDARD_XPAD_CONFIG["name"].encode("utf-8")[:UINPUT_MAX_NAME_SIZE - 1],
                0,
            ))
            fcntl.ioctl(fd, UI_DEV_CREATE)
        except OSError:
            os.close(fd)
            raise
        self.fd = fd
        self.device_nodes = self._resolve_device_nodes()
        return self.device_nodes

    def _resolve_device_nodes(self) -> list:
        """The /dev/input nodes the kernel registered for this device: event*
        always, js* as well when joydev is loaded."""
        buffer = bytearray(UINPUT_SYSNAME_LEN)
        try:
            fcntl.ioctl(self.fd, UI_GET_SYSNAME, buffer, True)
        except OSError:
            return []
        sysname = bytes(buffer).split(b"\0", 1)[0].decode("utf-8", "replace")
        if not sysname:
            return []
        sysdir = os.path.join(UINPUT_SYSFS_BASE, sysname)
        try:
            entries = os.listdir(sysdir)
        except OSError:
            return []
        return sorted(
            os.path.join("/dev/input", entry)
            for entry in entries
            if entry.startswith(("event", "js"))
        )

    def emit(self, ev_type: int, ev_code: int, ev_value: float) -> None:
        os.write(self.fd, get_evdev_events_packed(ev_type, ev_code, ev_value, LOCAL_ARCH_BITS))

    def destroy(self) -> None:
        if self.fd is None:
            return
        try:
            fcntl.ioctl(self.fd, UI_DEV_DESTROY)
        except OSError as e:
            logger_selkies_gamepad.warning(f"Gamepad {self.label}: could not destroy the kernel device: {e}")
        try:
            os.close(self.fd)
        except OSError:
            pass
        self.fd = None
        self.device_nodes = []


# Slot index -> SelkiesGamepad, process-wide (see SelkiesGamepad for why).
_persistent_gamepads: dict = {}


class SelkiesGamepad:
    """One virtual gamepad slot: interposer socket servers plus an optional
    kernel uinput device.

    Serves the joydev-style and evdev-style Unix sockets the Joystick
    Interposer preload connects applications to, fanning queued events out to
    every connected client and (when enabled) mirroring them onto a kernel
    uinput device.

    Instances live in `_persistent_gamepads` and outlive every per-service
    input handler: applications open the interposer sockets once at their own
    startup (the .so presents them as /dev/input devices), and a transport
    mode switch (websockets `<->` webrtc) tears one service down and starts the
    other, so closing or rebinding the sockets there would leave every
    running app holding a dead fd until it restarts. Process exit reclaims
    the fds; the next server start unlinks stale socket files before binding.

    Attributes:
        uinput_enabled: Kernel gamepads are on for this slot.
        uinput: The kernel device, created on first use so an unused slot is
            not a phantom controller in every application.
        mapper: Button/axis mapper for this pad, built by set_config.
        config_payload_cache: js_config_t payload handed to interposer clients,
            built by set_config.
        js_clients, evdev_clients: `{writer: {'arch_bits': bits}}`.
        events_queue: Bounded so one stalled client cannot grow it without
            limit and wedge delivery for every client; overflow drops the
            oldest event (see send_event).
        _held_controls: Client controls (is_button, index) currently driven
            non-neutral, so reset_state releases exactly what is held.
        _js_state: Last queued js value per (ev_type, number), the source for
            init_state_burst; updated at queue time so the snapshot stays
            truthful even for events the bounded queue drops.
    """

    def __init__(self, js_interposer_socket_path: str,
                 evdev_interposer_socket_path: str,
                 loop: Optional[asyncio.AbstractEventLoop] = None,
                 uinput_enabled: bool = False) -> None:
        self.js_sock_path = js_interposer_socket_path
        self.evdev_sock_path = evdev_interposer_socket_path
        self.loop = loop or asyncio.get_running_loop()

        self.uinput_enabled = uinput_enabled
        self.uinput = None
        
        self.mapper = None
        self.config_payload_cache = None

        self.js_server = None
        self.evdev_server = None
        self.js_clients = {}
        self.evdev_clients = {}
        
        self.events_queue = asyncio.Queue(maxsize=4096)
        self.running = False
        self._event_processor_task = None

        self._held_controls = set()
        self._js_state = {}

    def set_config(self, client_input_name: str, client_num_btns: int,
                   client_num_axes: int) -> None:
        """Build the mapper and cache the js_config_t payload served to interposer clients."""
        self.mapper = GamepadMapper(STANDARD_XPAD_CONFIG, client_input_name, client_num_btns, client_num_axes)
        
        js_idx = 0 
        match = re.search(r"selkies_js(\d+)\.sock$", self.js_sock_path)
        if match:
            js_idx = int(match.group(1))
        else:
            logger_selkies_gamepad.warning(
                f"Failed to parse js_index from {self.js_sock_path}, "
                f"defaulting to 0 for payload name generation if needed."
            )

        payload_controller_config = {
            "name": STANDARD_XPAD_CONFIG.get("name", f"Selkies Virtual JS{js_idx}"),
            "vendor_id": STANDARD_XPAD_CONFIG.get("vendor_id", 0x0000),
            "product_id": STANDARD_XPAD_CONFIG.get("product_id", 0x0000),
            "version": STANDARD_XPAD_CONFIG.get("version", 0x0114),
            "buttons": STANDARD_XPAD_CONFIG.get("btn_map", []), 
            "axes": STANDARD_XPAD_CONFIG.get("axes_map", [])
        }
        
        self.config_payload_cache = self._make_interposer_config_payload(js_idx, payload_controller_config)
        
        logger_selkies_gamepad.info(
            f"Gamepad configured. JS socket: {self.js_sock_path}, EVDEV socket: {self.evdev_sock_path}. "
            f"Using fixed config: {STANDARD_XPAD_CONFIG['name']}"
        )

    def ensure_uinput(self) -> None:
        """Bring this slot's kernel device up, once, if kernel gamepads are on.
        A failure downgrades the slot to the interposer sockets rather than
        killing input."""
        if not self.uinput_enabled or self.uinput is not None:
            return
        device = UInputGamepad(os.path.basename(self.js_sock_path))
        try:
            nodes = device.create()
        except OSError as e:
            self.uinput_enabled = False
            logger_selkies_gamepad.error(
                f"Gamepad {self.js_sock_path}: could not create a kernel device ({e}); "
                "this slot now reaches applications only through the Joystick Interposer."
            )
            return
        self.uinput = device
        logger_selkies_gamepad.info(
            f"Gamepad {self.js_sock_path}: kernel device ready ({', '.join(nodes) or 'node path unknown'})."
        )
        unreadable = [node for node in nodes if not os.access(node, os.R_OK)]
        if unreadable:
            logger_selkies_gamepad.warning(
                f"Gamepad {self.js_sock_path}: {', '.join(unreadable)} is not readable by this user, "
                "so applications cannot open it. Add the account to the 'input' group."
            )

    def _emit_uinput(self, ev_type: int, ev_code: int, ev_value: float) -> None:
        """Mirror one event onto the kernel device, tearing it down on write failure."""
        self.ensure_uinput()
        if self.uinput is None:
            return
        try:
            self.uinput.emit(ev_type, ev_code, ev_value)
        except OSError as e:
            logger_selkies_gamepad.error(
                f"Gamepad {self.js_sock_path}: kernel device write failed ({e}); tearing it down."
            )
            self.uinput.destroy()
            self.uinput = None
            self.uinput_enabled = False

    def _make_interposer_config_payload(self, js_index: int, controller_config: dict) -> bytes:
        """Create the js_config_t payload sent to the C interposer.

        The payload is always exactly C_INTERPOSER_STRUCT_SIZE bytes; every
        failure path returns a zeroed buffer of that size rather than raising,
        so a client handshake never dies on a malformed config.
        """
        struct_fmt = base_struct_fmt = "undefined"
        try:
            name_str = controller_config.get("name", f"Selkies Virtual JS{js_index}")
            name_bytes_utf8 = name_str.encode('utf-8')
            if len(name_bytes_utf8) >= CONTROLLER_NAME_MAX_LEN:
                name_bytes_for_pack = name_bytes_utf8[:CONTROLLER_NAME_MAX_LEN - 1] + b'\0'
            else:
                name_bytes_for_pack = name_bytes_utf8.ljust(CONTROLLER_NAME_MAX_LEN, b'\0')

            if len(name_bytes_for_pack) != CONTROLLER_NAME_MAX_LEN:
                 logging.error(f"CRITICAL: name_bytes_for_pack is not {CONTROLLER_NAME_MAX_LEN} bytes long! Got {len(name_bytes_for_pack)}")
                 return b'\0' * C_INTERPOSER_STRUCT_SIZE

            raw_vendor = controller_config.get("vendor_id")
            if isinstance(raw_vendor, str):
                vendor_id = int(raw_vendor, 16)
            elif isinstance(raw_vendor, int):
                vendor_id = raw_vendor
            else:
                vendor_id = 0x045e
            raw_product = controller_config.get("product_id")
            if isinstance(raw_product, str):
                product_id = int(raw_product, 16)
            elif isinstance(raw_product, int):
                product_id = raw_product
            else:
                product_id = 0x028e
            raw_version = controller_config.get("version")
            if isinstance(raw_version, str):
                version_id = int(raw_version, 16)
            elif isinstance(raw_version, int):
                version_id = raw_version
            else:
                version_id = 0x0114

            buttons_evdev_codes = controller_config.get("buttons", [])
            axes_evdev_codes = controller_config.get("axes", [])

            # Counts clamped to the array capacity: a count above the truncated
            # map length would drive an out-of-bounds read in the C interposer.
            num_actual_btns = min(len(buttons_evdev_codes), INTERPOSER_MAX_BTNS)
            num_actual_axes = min(len(axes_evdev_codes), INTERPOSER_MAX_AXES)

            padded_btn_map_for_pack = list(buttons_evdev_codes)
            if len(padded_btn_map_for_pack) > INTERPOSER_MAX_BTNS:
                logging.warning(f"Controller '{name_str}' has {len(padded_btn_map_for_pack)} buttons, truncating to {INTERPOSER_MAX_BTNS} for config.")
                padded_btn_map_for_pack = padded_btn_map_for_pack[:INTERPOSER_MAX_BTNS]
            else:
                padded_btn_map_for_pack.extend([0] * (INTERPOSER_MAX_BTNS - len(padded_btn_map_for_pack)))

            padded_axes_map_for_pack = list(axes_evdev_codes)
            if len(padded_axes_map_for_pack) > INTERPOSER_MAX_AXES:
                logging.warning(f"Controller '{name_str}' has {len(padded_axes_map_for_pack)} axes, truncating to {INTERPOSER_MAX_AXES} for config.")
                padded_axes_map_for_pack = padded_axes_map_for_pack[:INTERPOSER_MAX_AXES]
            else:
                padded_axes_map_for_pack.extend([0] * (INTERPOSER_MAX_AXES - len(padded_axes_map_for_pack)))

            base_struct_fmt = f"={CONTROLLER_NAME_MAX_LEN}sxHHHHH{INTERPOSER_MAX_BTNS}H{INTERPOSER_MAX_AXES}B"
            
            size_without_explicit_end_padding = struct.calcsize(base_struct_fmt)

            padding_needed = C_INTERPOSER_STRUCT_SIZE - size_without_explicit_end_padding

            if padding_needed < 0:
                logging.error(
                    f"CRITICAL STRUCT SIZE ERROR: Python base packed size ({size_without_explicit_end_padding}) "
                    f"is larger than C interposer expected size ({C_INTERPOSER_STRUCT_SIZE}). "
                    f"This means constants (MAX_BTNS, MAX_AXES, NAME_LEN) or field types/order "
                    f"differ between Python 'base_struct_fmt' and C 'js_config_t'."
                )
                return b'\0' * C_INTERPOSER_STRUCT_SIZE

            struct_fmt = f"{base_struct_fmt}{padding_needed}x"
            
            python_final_packed_size = struct.calcsize(struct_fmt)
            if python_final_packed_size != C_INTERPOSER_STRUCT_SIZE:
                logging.error(
                    f"CRITICAL FINAL PYTHON PACKED SIZE MISMATCH for js_config_t! "
                    f"C interposer expects: {C_INTERPOSER_STRUCT_SIZE}, "
                    f"Python struct.pack calculated final size: {python_final_packed_size} using format '{struct_fmt}'. "
                    f"This indicates an issue with padding calculation logic or the base_struct_fmt."
                )
                return b'\0' * C_INTERPOSER_STRUCT_SIZE

            logging.debug(f"Using final struct_fmt: '{struct_fmt}' for js_config, packing to size {python_final_packed_size}")

            payload_args = [
                name_bytes_for_pack,
                vendor_id,
                product_id,
                version_id,
                num_actual_btns,
                num_actual_axes,
            ]
            payload_args.extend(padded_btn_map_for_pack)
            payload_args.extend(padded_axes_map_for_pack)

            payload = struct.pack(struct_fmt, *payload_args)

            log_display_name = name_bytes_for_pack.split(b'\0',1)[0].decode('utf-8', errors='replace')
            logging.info(f"Packed js_config payload for '{name_str}' (js{js_index}): "
                         f"len={len(payload)} bytes. "
                         f"Name='{log_display_name}', "
                         f"Vendor=0x{vendor_id:04x}, Product=0x{product_id:04x}, Version=0x{version_id:04x}, "
                         f"Reported Buttons={num_actual_btns} (Array capacity: {INTERPOSER_MAX_BTNS}), "
                         f"Reported Axes={num_actual_axes} (Array capacity: {INTERPOSER_MAX_AXES})")
            
            if len(payload) != C_INTERPOSER_STRUCT_SIZE:
                logging.error(f"FINAL PAYLOAD SIZE MISMATCH AFTER PACKING! Expected {C_INTERPOSER_STRUCT_SIZE}, got {len(payload)}. This is very bad.")
                return b'\0' * C_INTERPOSER_STRUCT_SIZE
            return payload

        except struct.error as e:
            current_struct_fmt = struct_fmt if struct_fmt != "undefined" else base_struct_fmt
            logging.error(f"Error packing joystick config for js{js_index} with format '{current_struct_fmt}': {e}")
            config_to_log = controller_config if 'controller_config' in locals() else {}
            logging.error(f"Controller config was: {config_to_log}")
            return b'\0' * C_INTERPOSER_STRUCT_SIZE
        except Exception as e:
            config_to_log = controller_config if 'controller_config' in locals() else {}
            logging.exception(f"Unexpected error creating interposer config payload for js{js_index} with config {config_to_log}: {e}")
            return b'\0' * C_INTERPOSER_STRUCT_SIZE

    async def _handle_interposer_client(self, reader: asyncio.StreamReader,
                                        writer: asyncio.StreamWriter,
                                        is_evdev_socket: bool) -> None:
        """Per-client handshake and lifetime: send config, read the client's
        architecture byte, register the writer for event fan-out, then hold the
        connection open until shutdown or disconnect.

        A JS client first gets its current state replayed as INIT events
        (joydev semantics); the snapshot, its write and the registration share
        one loop step, so no broadcast can interleave and the client's first
        live event strictly follows its snapshot. evdev has no in-band INIT:
        those clients poll state through the interposer's ioctl emulation.
        """
        peername = writer.get_extra_info('peername')
        socket_type_str = "EVDEV" if is_evdev_socket else "JS"
        clients_dict = self.evdev_clients if is_evdev_socket else self.js_clients
        sock_path = self.evdev_sock_path if is_evdev_socket else self.js_sock_path
        log_prefix = f"Gamepad {sock_path} Client {peername} ({socket_type_str}):"
        logger_selkies_gamepad.info(f"{log_prefix} Handler started.")

        try:
            if not self.config_payload_cache:
                logger_selkies_gamepad.error(f"{log_prefix} Config payload not ready. Aborting handler.")
                return
            logger_selkies_gamepad.info(f"{log_prefix} Preparing to send config payload. Length: {len(self.config_payload_cache)}, Expected C size: {EXPECTED_C_STRUCT_SIZE}, First 16 bytes: {self.config_payload_cache[:16].hex()}")
            writer.write(self.config_payload_cache)
            await writer.drain()
            logger_selkies_gamepad.debug(f"{log_prefix} Sent config payload.")

            arch_byte = await reader.readexactly(1)
            client_sizeof_long = struct.unpack("=B", arch_byte)[0]
            client_arch_bits = client_sizeof_long * 8
            logger_selkies_gamepad.info(f"{log_prefix} Received arch specifier: {client_sizeof_long} bytes ({client_arch_bits}-bit).")

            if not is_evdev_socket:
                writer.write(self.init_state_burst())
            clients_dict[writer] = {'arch_bits': client_arch_bits}
            await writer.drain()
            logger_selkies_gamepad.info(f"{log_prefix} Added to active list. Total {socket_type_str} clients: {len(clients_dict)}.")

            while self.running and not writer.is_closing():
                await asyncio.sleep(0.1) 
            
            if not self.running:
                logger_selkies_gamepad.info(f"{log_prefix} Exiting handler normally because self.running is False.")
            if writer.is_closing():
                logger_selkies_gamepad.info(f"{log_prefix} Exiting handler normally because writer.is_closing() is True (client likely closed connection).")

        except (asyncio.IncompleteReadError, ConnectionResetError, BrokenPipeError) as e:
            logger_selkies_gamepad.info(f"{log_prefix} Disconnected (expected error): {type(e).__name__} - {e}")
        except Exception as e:
            logger_selkies_gamepad.error(f"{log_prefix} Unhandled error in handler: {e}", exc_info=True)
        finally:
            logger_selkies_gamepad.info(f"{log_prefix} Entering finally block.")
            if writer in clients_dict:
                del clients_dict[writer]
                logger_selkies_gamepad.info(f"{log_prefix} Removed from active list. Total {socket_type_str} clients now: {len(clients_dict)}.")
            else:
                logger_selkies_gamepad.warning(f"{log_prefix} Writer not found in active list during finally block.")

            if not writer.is_closing():
                logger_selkies_gamepad.info(f"{log_prefix} Explicitly closing writer in finally block.")
                writer.close()
                await writer.wait_closed()
            logger_selkies_gamepad.info(f"{log_prefix} Handler finished.")

    async def _run_single_server(self, interposer_socket_path: str,
                                 is_evdev_socket: bool) -> Optional[asyncio.AbstractServer]:
        """Bind one interposer Unix server (unlinking a stale socket file first);
        None on failure."""
        sock_dir = os.path.dirname(interposer_socket_path)
        if sock_dir and not os.path.exists(sock_dir):
            try: os.makedirs(sock_dir, exist_ok=True)
            except OSError as e:
                logger_selkies_gamepad.error(f"Failed to create directory {sock_dir} for socket: {e}")
                return None
        
        if os.path.exists(interposer_socket_path):
            try:
                os.unlink(interposer_socket_path)
                logger_selkies_gamepad.debug(f"Removed existing socket file: {interposer_socket_path}")
            except OSError as e:
                logger_selkies_gamepad.warning(f"Could not remove existing file at {interposer_socket_path}: {e}. Bind might fail.")

        try:
            server = await asyncio.start_unix_server(
                lambda r, w: self._handle_interposer_client(r, w, is_evdev_socket),
                path=interposer_socket_path
            )
            addr = server.sockets[0].getsockname() if server.sockets else interposer_socket_path
            logger_selkies_gamepad.info(f"{'EVDEV' if is_evdev_socket else 'JS'} interposer server listening on {addr}")
            return server
        except Exception as e:
            logger_selkies_gamepad.error(f"Failed to start {'EVDEV' if is_evdev_socket else 'JS'} server on {interposer_socket_path}: {e}", exc_info=True)
            return None

    async def run_servers(self) -> None:
        """Start both interposer servers and the event processor; runs until close()."""
        if not self.mapper:
            logger_selkies_gamepad.error("Mapper not set. Call set_config() before run_servers().")
            return

        self.running = True
        if self._event_processor_task is None or self._event_processor_task.done():
            self._event_processor_task = asyncio.create_task(self._process_event_queue())

        self.js_server = await self._run_single_server(self.js_sock_path, is_evdev_socket=False)
        self.evdev_server = await self._run_single_server(self.evdev_sock_path, is_evdev_socket=True)

        if not self.js_server and not self.evdev_server:
            logger_selkies_gamepad.error("Neither JS nor EVDEV interposer server could be started. Stopping.")
            self.running = False
            if self._event_processor_task and not self._event_processor_task.done():
                self._event_processor_task.cancel()
            return
        
        while self.running:
            await asyncio.sleep(1)
        logger_selkies_gamepad.info("run_servers loop exited.")

    def send_event(self, client_event_idx: int, client_value: float,
                   is_button_event: bool) -> None:
        """Map one client control change and queue it for fan-out to every client.

        On overflow the oldest queued event is dropped to make room — for a
        gamepad the freshest state matters and a stale sample is worthless —
        so a slowly draining client cannot back-pressure into unbounded
        growth; the shutdown sentinel (None) is re-enqueued if evicted.
        """
        if not self.mapper or not self.running:
            return
        event_package = self.mapper.get_mapped_events(client_event_idx, client_value, is_button_event)
        if event_package:
            control = (is_button_event, client_event_idx)
            if client_value:
                self._held_controls.add(control)
            else:
                self._held_controls.discard(control)
            js_data = event_package.get('js_event_data')
            if js_data:
                _, value, ev_type, number = struct.unpack("=IhBB", js_data)
                self._js_state[(ev_type, number)] = value
            logger_selkies_gamepad.debug(f"Gamepad {self.js_sock_path}: Queuing event: {event_package}")
            try:
                self.events_queue.put_nowait(event_package)
            except asyncio.QueueFull:
                try:
                    dropped = self.events_queue.get_nowait()
                    self.events_queue.task_done()
                    if dropped is None:
                        self.events_queue.put_nowait(None)
                        return
                except asyncio.QueueEmpty:
                    pass
                try:
                    self.events_queue.put_nowait(event_package)
                except asyncio.QueueFull:
                    logger_selkies_gamepad.warning(
                        f"Gamepad {self.js_sock_path}: event queue full; dropping event."
                    )

    def reset_state(self) -> None:
        """Emit a neutral value for every control still held non-neutral, so a
        dropped association leaves no stuck button or off-center axis behind on
        the app driving this pad."""
        for is_button_event, client_event_idx in list(self._held_controls):
            self.send_event(client_event_idx, 0, is_button_event)

    def init_state_burst(self) -> bytes:
        """joydev-parity state replay for a newly connected JS client: every
        control's current value as JS_EVENT_INIT-flagged events, so an app
        opening the pad mid-hold starts from the true state (and input during
        the connect handshake is covered as state, not lost edges). Buttons
        rest at 0, stick/hat axes at center, triggers at the mapper's released
        value."""
        mapping = STANDARD_XPAD_CONFIG["mapping"]
        parts = []
        for idx in range(len(STANDARD_XPAD_CONFIG["btn_map"])):
            value = self._js_state.get((JS_EVENT_BUTTON, idx), 0)
            parts.append(get_js_event_packed(JS_EVENT_BUTTON | JS_EVENT_INIT, idx, value))
        for idx in range(len(STANDARD_XPAD_CONFIG["axes_map"])):
            rest = normalize_axis_value(
                0,
                idx in mapping["trigger_internal_abstract_axis_indices"],
                idx in mapping["hat_internal_abstract_axis_indices"],
                for_js_event=True,
            )
            value = self._js_state.get((JS_EVENT_AXIS, idx), rest)
            parts.append(get_js_event_packed(JS_EVENT_AXIS | JS_EVENT_INIT, idx, value))
        return b"".join(parts)

    async def _process_event_queue(self) -> None:
        """Drain the event queue until the None sentinel, fanning each event out
        to JS, EVDEV and uinput consumers.

        Each client drain is bounded and a stalled client is closed, so a game
        that stops reading its socket cannot freeze delivery for the others.
        """
        logger_selkies_gamepad.info(f"Gamepad {self.js_sock_path}: Event processor started.")
        while self.running:
            try:
                event_package = await self.events_queue.get()
                if event_package is None:
                    self.events_queue.task_done()
                    break
                
                logger_selkies_gamepad.debug(f"Gamepad {self.js_sock_path}: Dequeued event: {event_package}")
                
                js_data = event_package.get('js_event_data')
                evdev_template = event_package.get('evdev_event_template') 

                if js_data:
                    for i, (writer, _client_info) in enumerate(list(self.js_clients.items())):
                        if not writer.is_closing():
                            try:
                                writer.write(js_data)
                                await asyncio.wait_for(writer.drain(), timeout=1.0)
                                logger_selkies_gamepad.debug(f"Gamepad {self.js_sock_path}: JS event drained to client #{i}.")
                            except asyncio.TimeoutError:
                                logger_selkies_gamepad.warning(f"Gamepad {self.js_sock_path}: JS client #{i} stalled; closing it.")
                                writer.close()
                            except (ConnectionResetError, BrokenPipeError): pass
                            except Exception as e:
                                logger_selkies_gamepad.error(f"Error sending to JS client #{i}: {e}", exc_info=True)
                
                if evdev_template:
                    ev_type, ev_code, ev_value = evdev_template
                    self._emit_uinput(ev_type, ev_code, ev_value)
                    for i, (writer, client_info) in enumerate(list(self.evdev_clients.items())):
                        if not writer.is_closing():
                            try:
                                client_arch_bits = client_info.get('arch_bits', 64) 
                                evdev_data = get_evdev_events_packed(ev_type, ev_code, ev_value, client_arch_bits)
                                writer.write(evdev_data)
                                await asyncio.wait_for(writer.drain(), timeout=1.0)
                                logger_selkies_gamepad.debug(f"Gamepad {self.js_sock_path}: EVDEV event drained to client #{i}.")
                            except asyncio.TimeoutError:
                                logger_selkies_gamepad.warning(f"Gamepad {self.js_sock_path}: EVDEV client #{i} stalled; closing it.")
                                writer.close()
                            except (ConnectionResetError, BrokenPipeError): pass
                            except Exception as e:
                                logger_selkies_gamepad.error(f"Error sending to EVDEV client #{i}: {e}", exc_info=True)
                
                self.events_queue.task_done()
            except asyncio.CancelledError:
                logger_selkies_gamepad.info(f"Gamepad {self.js_sock_path}: Event processor task cancelled.")
                break
            except Exception as e:
                logger_selkies_gamepad.error(f"Gamepad {self.js_sock_path}: Unhandled error in event processor: {e}", exc_info=True)
        logger_selkies_gamepad.info(f"Gamepad {self.js_sock_path}: Event processor stopped.")


    async def close(self) -> None:
        """Stop servers, drop clients, unlink socket files, destroy the kernel device."""
        logger_selkies_gamepad.info(f"Closing gamepad services for JS:{self.js_sock_path}, EVDEV:{self.evdev_sock_path}")
        self.running = False

        if self.js_server:
            self.js_server.close()
            await self.js_server.wait_closed()
            self.js_server = None
            logger_selkies_gamepad.info(f"JS interposer server {self.js_sock_path} closed.")
        if self.evdev_server:
            self.evdev_server.close()
            await self.evdev_server.wait_closed()
            self.evdev_server = None
            logger_selkies_gamepad.info(f"EVDEV interposer server {self.evdev_sock_path} closed.")

        for writer in list(self.js_clients.keys()):
            if not writer.is_closing(): writer.close()
        self.js_clients.clear()
        for writer in list(self.evdev_clients.keys()):
            if not writer.is_closing(): writer.close()
        self.evdev_clients.clear()
        
        if self._event_processor_task and not self._event_processor_task.done():
            try:
                self.events_queue.put_nowait(None) 
                await asyncio.wait_for(self._event_processor_task, timeout=2.0)
            except asyncio.TimeoutError:
                logger_selkies_gamepad.warning("Event processor task timed out on close, cancelling.")
                self._event_processor_task.cancel()
            except asyncio.CancelledError:
                pass 
            except Exception as e:
                logger_selkies_gamepad.error(f"Exception stopping event processor: {e}")
        self._event_processor_task = None
        
        for sock_path in [self.js_sock_path, self.evdev_sock_path]:
            if sock_path and os.path.exists(sock_path):
                try:
                    os.unlink(sock_path)
                    logger_selkies_gamepad.info(f"Removed socket file: {sock_path}")
                except OSError as e:
                    logger_selkies_gamepad.warning(f"Could not remove socket file {sock_path} on close: {e}")

        if self.uinput is not None:
            self.uinput.destroy()
            self.uinput = None

        logger_selkies_gamepad.info("Gamepad services fully closed.")


# Watch tasks for client-requested commands; referenced so they cannot be
# garbage-collected mid-watch.
_command_watch_tasks: set = set()
# Pids of the client-requested commands launched here and still running.
_launched_command_pids: set = set()

# proot-apps launch prefixes by the windowing system the session's apps run
# on: terminal plus its run flag (st and foot take the command bare, xterm
# wants -e). The first installed one is published to clients as app_terminal.
X11_APP_TERMINALS = ("st", "xterm -e")
WAYLAND_APP_TERMINALS = ("foot", "st", "xterm -e")

# Session environment a launch from outside must adopt from a session process:
# the session bus above all (dbus-launch/dbus-run-session addresses live only
# there), plus the desktop identity toolkits and xdg-desktop-portal read.
SESSION_ENV_ADOPTED = ("DBUS_SESSION_BUS_ADDRESS", "XAUTHORITY", "XDG_CURRENT_DESKTOP",
                       "DESKTOP_SESSION", "XDG_SESSION_DESKTOP", "XDG_MENU_PREFIX",
                       "XDG_SESSION_CLASS", "XDG_SESSION_ID")


def live_x_displays() -> list:
    """Local X display names (":N") with a server accepting connections,
    lowest number first."""
    found = []
    try:
        for name in os.listdir("/tmp/.X11-unix"):
            if name.startswith("X") and name[1:].isdigit():
                disp = ":" + name[1:]
                if x_display_live(disp):
                    found.append(int(name[1:]))
    except OSError:
        pass
    return [f":{n}" for n in sorted(found)]


def _proc_is_xwayland(pid: int, display: str) -> bool:
    """Whether ``pid`` is an Xwayland of this user serving ``display`` (its
    basename is Xwayland and ``display`` is one of its argv tokens)."""
    try:
        if os.stat(f"/proc/{pid}").st_uid != os.getuid():
            return False
        with open(f"/proc/{pid}/cmdline", "rb") as f:
            argv = f.read().split(b"\0")
    except (OSError, ValueError):
        return False
    if not argv or not argv[0]:
        return False
    want = display.encode()
    return (os.path.basename(argv[0]).split(b"/")[-1] == b"Xwayland"
            and any(a == want for a in argv[1:]))


_xwayland_pid_cache: dict = {}


def x_display_is_xwayland(display: str) -> bool:
    """True when the live X server on local display ``display`` is a rootful
    Xwayland of this user — an X11 desktop hosted directly on the capture
    compositor, whose selection nothing bridges — rather than a plain Xvfb/Xorg
    that merely happens to hold the display number. Cached per display while its
    serving process lives, so a `cmd` burst does not rescan /proc each time."""
    display = (display or "").strip()
    if not display:
        return False
    cached = _xwayland_pid_cache.get(display)
    if cached is not None:
        if _proc_is_xwayland(cached, display):
            return True
        _xwayland_pid_cache.pop(display, None)
    try:
        pids = [int(n) for n in os.listdir("/proc") if n.isdigit()]
    except OSError:
        return False
    for pid in pids:
        if _proc_is_xwayland(pid, display):
            _xwayland_pid_cache[display] = pid
            return True
    return False


def dbus_address_live(address: str) -> bool:
    """True when the first transport of a D-Bus address is a Unix socket that
    accepts a connection, or is not a Unix socket at all (unverifiable, kept)."""
    first = address.split(";", 1)[0]
    if not first.startswith("unix:"):
        return True
    params = dict(kv.split("=", 1) for kv in first[len("unix:"):].split(",") if "=" in kv)
    if "path" in params:
        target = params["path"]
    elif "abstract" in params:
        target = "\0" + params["abstract"]
    else:
        return True
    try:
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.settimeout(0.2)
        try:
            sock.connect(target)
            return True
        finally:
            sock.close()
    except OSError:
        return False


def _descendants(roots: Iterable[int]) -> set:
    """The given pids plus every live descendant of theirs (from /proc/*/stat)."""
    children: dict = {}
    try:
        for name in os.listdir("/proc"):
            if not name.isdigit():
                continue
            try:
                with open(f"/proc/{name}/stat") as f:
                    stat = f.read()
                ppid = int(stat.rsplit(")", 1)[1].split()[1])
            except (OSError, ValueError, IndexError):
                continue
            children.setdefault(ppid, []).append(int(name))
    except OSError:
        return set()
    out, todo = set(roots), list(roots)
    while todo:
        for child in children.get(todo.pop(), []):
            if child not in out:
                out.add(child)
                todo.append(child)
    return out


def _process_environ(pid: int) -> Optional[dict]:
    """The environment of a process from /proc, or None when unreadable."""
    try:
        with open(f"/proc/{pid}/environ", "rb") as f:
            raw = f.read()
    except OSError:
        return None
    env = {}
    for item in raw.split(b"\0"):
        if b"=" in item:
            k, v = item.split(b"=", 1)
            env[k.decode("utf-8", "replace")] = v.decode("utf-8", "replace")
    return env


def session_environment(x11_display: Optional[str], wayland_display: Optional[str]) -> dict:
    """The SESSION_ENV_ADOPTED subset of the environment of the desktop session
    running on the given display(s), read from the oldest process of this user
    that carries a session bus address and exports one of those displays. Empty
    when no such process exists (the session has not started, or runs no bus).

    A Wayland socket name is relative to XDG_RUNTIME_DIR, so it identifies a
    session only together with it; an X display name is host-wide and outlives
    the server that used it, so a process of this runtime dir ranks before a
    leftover of an earlier session. Commands launched from here (and their
    descendants) carry whatever was adopted for them and are never a source.
    """
    if not x11_display and not wayland_display:
        return {}
    uid = os.getuid()
    me = os.getpid()
    runtime_dir = os.environ.get("XDG_RUNTIME_DIR")
    candidates = []
    try:
        pids = [int(n) for n in os.listdir("/proc") if n.isdigit()]
    except OSError:
        return {}
    own = _descendants(_launched_command_pids) if _launched_command_pids else set()
    for pid in pids:
        if pid == me or pid in own:
            continue
        try:
            if os.stat(f"/proc/{pid}").st_uid != uid:
                continue
        except OSError:
            continue
        env = _process_environ(pid)
        if not env or not env.get("DBUS_SESSION_BUS_ADDRESS"):
            continue
        same_runtime = not runtime_dir or env.get("XDG_RUNTIME_DIR") == runtime_dir
        same_x = bool(x11_display) and env.get("DISPLAY") == x11_display
        same_wl = bool(wayland_display) and env.get("WAYLAND_DISPLAY") == wayland_display and same_runtime
        if same_x or same_wl:
            candidates.append((0 if same_runtime else 1, pid, env))
    for _, _, env in sorted(candidates, key=lambda c: c[:2]):
        if dbus_address_live(env["DBUS_SESSION_BUS_ADDRESS"]):
            return {k: env[k] for k in SESSION_ENV_ADOPTED if env.get(k)}
    return {}


def first_installed(commands: Iterable[str]) -> Optional[str]:
    """The first command prefix whose executable (its first word) is installed."""
    for command in commands:
        words = command.split()
        if words and shutil.which(words[0]):
            return command
    return None


async def run_client_command(command_to_run: str, logger: logging.Logger,
                             notify: Optional[Callable[[str], Any]] = None,
                             env: Optional[dict] = None) -> None:
    """Launch a client-requested command and watch it to completion.

    ``env`` is the environment the command runs in — the session's (see
    WebRTCInput.app_launch_env), so a launched application lands on the
    display and session bus the desktop uses; None inherits the server's.
    Output stays discarded (a long-running app must never block on a full
    pipe), but a launch failure or any nonzero exit — above all 127, the
    command-not-installed case — is reported through ``notify`` (async, one
    text argument) with the runtime and the echoed command, because the
    dashboards' apps UI marks the action done optimistically and needs a
    counter-signal to roll back. A clean exit stays unreported, and the
    dashboards decide relevance by age: a long-lived app quitting nonzero
    hours later is application lifecycle, not a launch failure.
    """
    try:
        process = await subprocess.create_subprocess_shell(
            command_to_run,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            cwd=os.path.expanduser("~"),
            env=env,
        )
    except Exception as e:
        logger.error(f"Failed to launch command '{command_to_run}': {e}")
        if notify:
            try:
                await notify(f"failed to launch: {command_to_run}")
            except Exception:
                pass
        return
    logger.info(f"Launched command '{command_to_run}' with PID {process.pid}")
    _launched_command_pids.add(process.pid)

    started = time.monotonic()

    async def _watch():
        try:
            code = await process.wait()
        except Exception:
            return
        finally:
            _launched_command_pids.discard(process.pid)
        if not code:
            return
        runtime = time.monotonic() - started
        hint = " (command not found)" if code in (126, 127) else ""
        logger.warning(
            f"Command '{command_to_run}' exited with status {code}{hint} after {runtime:.1f}s")
        if notify:
            try:
                await notify(
                    f"exited with status {code}{hint} after {runtime:.1f}s: {command_to_run}")
            except Exception:
                pass

    task = asyncio.create_task(_watch())
    _command_watch_tasks.add(task)
    task.add_done_callback(_command_watch_tasks.discard)


class WebRTCInputError(Exception):
    """Raised for input-handler failures surfaced to the transport layer."""

class WebRTCInput:
    """The server-side input authority shared by both transports.

    Dispatches every client data-channel/WebSocket message: keyboard, mouse,
    gamepad, clipboard (including multipart transfers), cursor monitoring, and
    the settings/stats callbacks the streaming pipeline registers on it. One
    instance exists per transport service; X11 and Wayland sessions flow
    through the same handler so behavior stays in parity, with the backend
    chosen by is_wayland.

    Keyboard injection follows the module-level fallback ladders. On Wayland
    all key work is serialized through keyboard_queue and a single worker so
    key ordering holds across the seat, virtual-keyboard, and clipboard rungs;
    on X11 injection is direct (XTEST first, xdotool fallback) with stale-key
    sweeping and server-side auto-repeat emulation.

    Attributes:
        app_wayland_display: Socket of the compositor apps run under (input and
            clipboard target) when it differs from the pixelflux capture
            compositor; resolved lazily into `_app_wl_display_cached`, since a
            nested session comes up after this process.
        _app_wl_negcache, _app_wl_negcache_at: Negative cache for the app
            compositor auto-detect sweep, consulted from per-key native
            injection, so a TTL floors the directory relist until a distinct
            compositor appears.
        _app_wl_is_separate: True once a resolved app compositor is confirmed
            distinct from the capture compositor, so
            `_has_separate_app_compositor` answers without re-resolving.
        _x_event_wake, _x_watcher_fd: Event-driven wake for the X consumers
            (cursor monitor, keymap watch): a loop reader on the input
            connection's fd sets the Event, so those loops block with zero
            wakeups instead of polling the socket.
        LEVEL_MODIFIER_KEYSYMS: Shift_L/R, ISO_Level3_Shift, Mode_switch — the
            level-selecting modifiers whose client-held state the injectors
            consult in place of a per-press server query.
        MODIFIER_KEYSYMS: Keysyms never armed for auto-repeat or routed as
            ordinary keys. Super and Hyper are included because the client
            maps the Meta/Windows key to Super, not Meta.
        gamepad_heartbeats: Slot to last held-state heartbeat (`js,h`). Only
            slots that have sent one are swept: a client that never heartbeats
            (an older web client) keeps the transport-close release path alone.
        uinput_gamepads: Resolved once so the decision and its reason are
            logged at startup rather than per slot.
        _binary_clipboard_lock: Serializes update_binary_clipboard_setting so
            its cancel+reassign of the monitor task is atomic under rapid
            toggles.
        _clipboard_monitor_active: Singleton guard; one start_clipboard loop
            runs at a time.
        _cursor_msg_cache: (serial, size cap) to encoded cursor message, see
            `_encode_cursor`.
        _client_kb_layout: Last client keyboardLayout hint seen (SETTINGS) on
            either transport.
        _wl_seat_client_layout: The hint the Wayland seat's base layout
            currently carries; None while the seat is on the deployment layout.
        _wl_keymap_owner: Keysym policy for the seat, kept here rather than in
            the compositor: built lazily from the compositor's keymap, retried
            on a cooldown (`_wl_keymap_retry_at`) if that read fails, and
            rebuilt carrying held keys once a base-layout change sets
            `_wl_keymap_stale`.
        _wl_typer_lock, _wl_typer_retry_at: One-shot zwp_virtual_keyboard_v1
            injection via pixelflux, serialized so text commits keep their
            order; a compositor lacking the protocol is re-probed on a cooldown
            only, so per-keystroke fallbacks stay cheap.
        _clipboard_inject_lock, _clipboard_inject_active: Clipboard-paste text
            injection (the KWin route), serialized so one
            save/write/paste/restore cycle finishes before the next, with a
            reentrancy latch so the paste chord's own key path cannot recurse.
        _clipboard_last_bytes: Change-detection baseline shared by the monitor
            and write_clipboard: content this server just wrote is never
            re-broadcast (client/server echo loop), and the baseline survives
            client reconnects so nothing is resent unchanged.
        _app_watch_failure, _app_clip_read_failure: Last (display, error) the
            app-compositor selection watch / read failed with, so a persistent
            failure is reported once, not per tick.
        _wl_native_arm_failure: Last error the compositor clipboard callback
            failed to arm with; the monitor retries each tick and reports each
            distinct error once.
        _session_env_cache, _session_env_empty_at: Session environment adopted
            for application launches, keyed by the (x11_display,
            wayland_display) pair it was read for; the negative cache holds
            the time of the last empty scan per key so a command burst on a
            session with no bus does not rescan /proc each time.
        _last_clipboard_request_ts: REQUEST_CLIPBOARD (Ctrl/Cmd+C) debounce,
            keyed per requesting connection so a keypress storm cannot stack
            clipboard reads and one client's copy cannot suppress another's.
        _bg_tasks: Strong refs for fire-and-forget tasks; asyncio holds running
            tasks weakly, so an unreferenced one can be collected mid-flight.
        _wl_text_routed: Keysyms whose kd became buffered text on the Wayland
            worker (nested app compositor): their ku is swallowed, not released.
        pressed_keys: Keysym to last `kh` heartbeat; the sweep auto-releases
            any key whose heartbeat stops (a key-up lost to congestion).
        reaped_atomic_keys: Atomic (non-alpha) keys the sweep reaped were never
            physically held, so a late ku would emit a spurious keyup; tracked
            to swallow it, and a fresh kd clears the entry.
        max_pressed_keys: Cap so a kd flood cannot grow `pressed_keys` unbounded.
        key_stale_window: Clients heartbeat every 100 ms but hidden tabs
            throttle to >= 1 s, so 2 s avoids false-releasing a backgrounded
            held key.
        key_repeat_enabled: Server-side auto-repeat, X11 only: XTEST/xdotool
            synthetic presses do not trigger the server's native repeat, so a
            held key would emit one character. Off on Wayland, where the
            focused app repeats virtual-keyboard keys itself via wl_keyboard
            repeat_info and a server-side repeat would double it.
        key_repeat_delay, key_repeat_interval, key_repeat_tick: Hold before
            the first repeat, spacing between repeats (~25 Hz) and the repeat
            loop's poll period; the first two adopt the X server's own values.
        key_repeat_heartbeat_grace: Repeat pauses when the held key's last
            heartbeat is older than this (stalled stream / hidden tab); kept
            above ~3x the client's 100 ms heartbeat.
        key_repeat_state: Keysym to the monotonic time of its next due repeat.
        keymap_watch_task: MappingNotify consumer for sessions where the
            cursor monitor (the normal X event consumer) is disabled.
    """

    def __init__(
        self,
        rtc_app: Any,
        uinput_mouse_socket_path: str = "",
        js_socket_path_prefix: str = "/tmp",
        enable_clipboard: str = "",
        enable_binary_clipboard: str = "",
        enable_cursors: bool = True,
        cursor_size: int = 16,
        cursor_scale: float = 1.0,
        cursor_debug: bool = False,
        max_cursor_size: int = 32,
        data_server_instance: Any = None,
        upload_dir: Optional[str] = None,
        is_wayland: bool = False,
        wayland_socket_index: int = 0,
        app_wayland_display: str = "",
        uinput_gamepad: str = "auto",
    ) -> None:
        self.wayland_socket_index = wayland_socket_index
        self.app_wayland_display = app_wayland_display
        self._app_wl_display_cached = None
        self._x_reconnect_thread = None
        self._x11_monitor_build_lock = asyncio.Lock()
        self._x_event_wake = None
        self._x_watcher_fd = None
        self._app_wl_negcache = None
        self._app_wl_negcache_at = 0.0
        self._app_wl_is_separate = False
        self.active_shortcut_modifiers = set()
        self.SHORTCUT_MODIFIER_XKEY_NAMES = {
            'Control_L', 'Control_R', 
            'Alt_L', 'Alt_R', 
            'Super_L', 'Super_R',
            'Meta_L', 'Meta_R'
        }
        self.active_modifiers = set()
        self.atomically_typed_keys = set()
        self.translated_keys = set()
        self.ACTION_MODIFIER_KEYSYMS = {65507, 65508, 65513, 65514, 65511, 65512,
                                        65515, 65516, 65517, 65518}
        self.LEVEL_MODIFIER_KEYSYMS = frozenset({0xFFE1, 0xFFE2, 0xFE03, 0xFF7E})
        self.MODIFIER_KEYSYMS = {
            # Shift_L, Shift_R
            65505, 65506,
            # Control_L, Control_R
            65507, 65508,
            # Alt_L, Alt_R
            65513, 65514,
            # ISO_Level3_Shift (AltGr)
            65027,
            # Meta_L, Meta_R
            65511, 65512,
            # Super_L, Super_R
            65515, 65516,
            # Hyper_L, Hyper_R
            65517, 65518,
        }
        self.rtc_app = rtc_app
        self.loop = asyncio.get_running_loop()
        self.js_socket_path_prefix = js_socket_path_prefix
        self.num_gamepads = 4
        self.gamepad_instances = {}
        self.client_gamepad_associations = {}
        self.gamepad_heartbeats = {}
        self.uinput_gamepads = uinput_gamepads_enabled(uinput_gamepad)

        self.clipboard_running = False
        self._binary_clipboard_lock = asyncio.Lock()
        self._clipboard_monitor_active = False
        self.uinput_mouse_socket_path = uinput_mouse_socket_path
        self.uinput_mouse_socket = None
        self.enable_clipboard = enable_clipboard
        self.enable_binary_clipboard = enable_binary_clipboard
        self.enable_cursors = enable_cursors
        self.cursors_running = False
        self.cursor_scale = cursor_scale
        self.cursor_size = cursor_size
        self.cursor_debug = cursor_debug
        # An explicit cursor_size raises the capture cap so the requested size
        # survives the transport instead of being resized down.
        if isinstance(cursor_size, int) and cursor_size > 0:
            max_cursor_size = max(max_cursor_size, cursor_size)
        self.max_cursor_size = max_cursor_size
        self.system_dpi = 96.0
        self.cursor_size_cap = max_cursor_size
        self._cursor_msg_cache = None
        self.keyboard = None
        self.mouse = None
        self.xdisplay = None
        self.button_mask = 0
        self.last_x = -1
        self.last_y = -1
        self.tracked_position_stale = False
        self.ping_start = None

        self.upload_dir = upload_dir
        self.upload_dir_path = None

        async def _unhandled_video_bitrate(bitrate, display_id="primary"):
            logger_webrtc_input.warning(f"unhandled on_video_encoder_bit_rate: {bitrate}")
        self.on_video_encoder_bit_rate = _unhandled_video_bitrate
        async def _unhandled_audio_bitrate(bitrate):
            logger_webrtc_input.warning(f"unhandled on_audio_encoder_bit_rate: {bitrate}")
        self.on_audio_encoder_bit_rate = _unhandled_audio_bitrate
        async def _unhandled_mouse_pointer(visible):
            logger_webrtc_input.warning(f"unhandled on_mouse_pointer_visible: {visible}")
        self.on_mouse_pointer_visible = _unhandled_mouse_pointer
        self.on_clipboard_read = self._on_clipboard_read
        self.on_set_fps = lambda fps, display_id="primary": logger_webrtc_input.warning("unhandled on_set_fps")
        self.on_request_keyframe = lambda display_id="primary": logger_webrtc_input.warning("unhandled on_request_keyframe")
        self.on_set_enable_resize = lambda enable_resize, res: logger_webrtc_input.warning("unhandled on_set_enable_resize")
        self.on_client_fps = lambda fps: logger_webrtc_input.warning("unhandled on_client_fps")
        self.on_client_latency = lambda latency: logger_webrtc_input.warning("unhandled on_client_latency")
        self.on_resize = lambda res, display_id="primary": logger_webrtc_input.warning("unhandled on_resize")
        self.on_scaling_ratio = lambda res: logger_webrtc_input.warning("unhandled on_scaling_ratio")
        self.on_ping_response = lambda latency: logger_webrtc_input.warning("unhandled on_ping_response")
        self.on_cursor_change = self._on_cursor_change
        async def _unhandled_webrtc_stats(webrtc_stat_type, webrtc_stats):
            logger_webrtc_input.debug(f"unhandled on_client_webrtc_stats: {webrtc_stat_type}")
        self.on_client_webrtc_stats = _unhandled_webrtc_stats
        self.clipboard_monitor_task = None
        self.multipart_clipboard_buffer = None
        self.multipart_clipboard_mime_type = "text/plain"
        self.multipart_clipboard_total_size = 0
        self.multipart_clipboard_in_progress = False
        self.multipart_clipboard_id = None
        self.multipart_clipboard_kind = None
        self.data_server_instance = data_server_instance
        self.on_update_settings = lambda settings_json, display_id="primary": logger_webrtc_input.warning("unhandled update_settings")
        self.is_wayland = is_wayland
        self.wayland_input = None
        self._client_kb_layout = None
        self._wl_seat_client_layout = None
        self._wl_keymap_owner = None
        self._wl_keymap_stale = False
        self._wl_keymap_owner_lock = asyncio.Lock()
        self._wl_keymap_retry_at = 0.0
        self._wl_typer_lock = asyncio.Lock()
        self._wl_typer_retry_at = 0.0
        self._clipboard_inject_lock = asyncio.Lock()
        self._clipboard_inject_active = False
        self._clipboard_last_bytes = None
        self._x11_clipboard_monitor = None
        self._x11_monitor_retry_at = 0.0
        self._x11_monitor_unavail_logged = False
        self._app_watch_failure = None
        self._app_clip_read_failure = None
        self._wl_native_arm_failure = None
        self._session_env_cache = {}
        self._session_env_empty_at = {}
        self._session_env_negcache_ttl = 5.0
        self._xclip_missing_warned = False
        self._last_clipboard_request_ts = {}
        self._clipboard_request_debounce = 0.25
        self._bg_tasks = set()
        self.keyboard_queue = asyncio.Queue(maxsize=4096)
        self.keyboard_worker_task = None
        self._wl_text_routed = {}
        self.pressed_keys = {}
        self.reaped_atomic_keys = set()
        self.max_pressed_keys = 1024
        self.key_stale_window = 2.0
        self.key_sweep_interval = 0.1
        self.key_sweep_task = None
        self.key_repeat_enabled = not self.is_wayland
        self.key_repeat_delay = 0.5
        self.key_repeat_interval = 0.04
        self.key_repeat_tick = 0.02
        self.key_repeat_heartbeat_grace = 0.3
        self.key_repeat_state = {}
        self.key_repeat_task = None
        self.keymap_watch_task = None
        self.on_update_rate_control_mode = lambda mode, display_id="primary": logger_webrtc_input.warning("unhandled on_update_rate_control_mode")
        self.on_update_crf = lambda value, display_id="primary": logger_webrtc_input.warning("unhandled on_update_crf")

        if self.is_wayland:
            try:
                if ScreenCapture is None:
                    raise RuntimeError("pixelflux is not installed")
                self.wayland_input = ScreenCapture()
                logger_webrtc_input.info("Wayland input injection initialized.")
                missing = [m for m in (
                    "clipboard_write_app", "clipboard_unwatch_app",
                    "list_outputs", "create_output", "set_keymap_overlay",
                    "hold_spare_app_screens", "set_app_output_scale",
                    "set_app_screen_geometry",
                    "set_app_wayland_display", "type_text_wayland",
                    "get_keyboard_state",
                ) if not hasattr(self.wayland_input, m)]
                if missing:
                    logger_webrtc_input.warning(
                        "Installed pixelflux is missing APIs this build "
                        "expects; Wayland features that depend on them "
                        "degrade or stay off. Update pixelflux to a "
                        "matching build.")
                    logger_webrtc_input.debug(
                        f"pixelflux methods absent: {', '.join(missing)}")
            except Exception as e:
                logger_webrtc_input.error(f"Failed to initialize Wayland input: {e}")

    async def _on_clipboard_read(self, data: Union[str, bytes],
                                 mime_type: str = "text/plain") -> None:
        await self.send_clipboard_data(data, mime_type)
    def _on_cursor_change(self, data: dict) -> None: self.send_cursor_data(data)
    async def send_clipboard_data(self, data: Union[str, bytes],
                                  mime_type: str = "text/plain",
                                  reply_to: Optional[str] = None,
                                  conn_id: Any = None) -> None:
        """Route clipboard content to the transport's own chunked sender.

        Each transport owns one (SelkiesStreamingApp.send_ws_clipboard_data /
        RTCApp.send_clipboard_data) and addresses the requester by the
        identity its own connections carry.
        """
        if self._ws_transport():
            await self.rtc_app.send_ws_clipboard_data(data, mime_type, reply_to=reply_to,
                                                      conn_id=conn_id)
        else:
            await self.rtc_app.send_clipboard_data(data, mime_type, reply_to=reply_to,
                                                   peer_id=conn_id)
    def _ws_transport(self) -> bool:
        """Whether the owning app is the websockets transport (it carries a
        `mode`); the WebRTC app has no such attribute and routes the other way."""
        return getattr(self.rtc_app, "mode", None) == "websockets"

    def send_cursor_data(self, data: dict) -> None:
        if self._ws_transport(): self.rtc_app.send_ws_cursor_data(data)
        else: self.rtc_app.send_cursor_data(data)
    def send_command_error(self, text: str, conn_id: Optional[str] = None) -> None:
        """Route a command failure notice to the transport (see send_cursor_data).

        Each transport carries the system action on its own wire format. Over
        WebRTC ``conn_id`` is the requesting peer's id, so the notice targets
        that peer's channel; the websockets transport notifies its requesting
        socket in its own cmd branch and only broadcasts here.
        """
        try:
            if self._ws_transport():
                self.rtc_app.send_system_action(f"command_error,{text}")
            else:
                self.rtc_app.send_system_action(f"command_error,{text}", peer_id=conn_id)
        except Exception:
            logger_webrtc_input.debug("command_error notify failed", exc_info=True)

    def __keyboard_connect(self) -> None: self.keyboard = _XTestKeyboard(self.xdisplay) if self.xdisplay else None

    def _apply_input_x_reply_bound(self) -> None:
        """Bound the wait for an X REPLY on the shared input connection so an
        unresponsive server (driver hang, a foreign client's server grab) raises
        ConnectionClosedError instead of freezing the event loop forever. XTEST
        injection (mouse motion/buttons, key press) is a no-reply request and is
        unaffected — only the modifier query (query_keymap) and the cursor-image
        fetch block, and those recover via _reconnect_xdisplay(). Event waits stay
        unbounded (a quiet server sending no cursor events is not an error)."""
        if self.xdisplay is None:
            return
        try:
            self.xdisplay.display.blocking_timeout = INPUT_X_REPLY_TIMEOUT_S
        except Exception:
            pass

    def _arm_x_event_watcher(self) -> None:
        """(Re)register the event-loop reader that wakes X consumers when the
        input connection's socket goes readable. Idempotent; re-arms when the fd
        changes under us (reconnect)."""
        if self._x_event_wake is None:
            return
        fd = None
        if self.xdisplay is not None:
            try:
                fd = self.xdisplay.fileno()
            except Exception:
                fd = None
        if fd is not None and fd < 0:
            fd = None
        if fd == self._x_watcher_fd:
            return
        if self._x_watcher_fd is not None:
            try:
                self.loop.remove_reader(self._x_watcher_fd)
            except Exception:
                pass
        self._x_watcher_fd = fd
        if fd is not None:
            try:
                self.loop.add_reader(fd, self._x_event_wake.set)
            except Exception as e:
                logger_webrtc_input.debug(f"X event watcher unavailable ({e}); consumers poll.")
                self._x_watcher_fd = None

    def _disarm_x_event_watcher(self) -> None:
        if self._x_watcher_fd is not None:
            try:
                self.loop.remove_reader(self._x_watcher_fd)
            except Exception:
                pass
            self._x_watcher_fd = None

    async def _wait_x_event(self, timeout: float = 1.0) -> None:
        """Sleep until the X socket signals readability or the failsafe elapses.
        Callers clear the Event BEFORE re-checking event availability, so an
        event arriving between the check and the wait is never lost. With no
        reader armed (add_reader unsupported or failed) nothing will ever set the
        Event, so fall back to a short poll rather than idling out the failsafe."""
        wake = self._x_event_wake
        if wake is None:
            await asyncio.sleep(timeout)
            return
        if self._x_watcher_fd is None:
            await asyncio.sleep(min(timeout, INPUT_X_EVENT_POLL_S))
            return
        try:
            await asyncio.wait_for(wake.wait(), timeout)
        except asyncio.TimeoutError:
            pass

    def _reconnect_xdisplay(self) -> None:
        """Rebuild the input X connection after a bounded reply-wait closed it.

        Fire-and-forget: the rebuild runs on a worker thread and installs via
        the loop (_install_reconnected_xdisplay), so xdisplay stays None — and
        input degraded to the xdotool fallbacks — until the attempt lands;
        callers just continue and the next X failure retries. The thread is
        needed because every caller sits on the event loop, and against a
        server hung under another client's grab — the main condition this
        reconnect exists to survive — close() and the connection setup would
        freeze the loop for the whole outage. The handshake itself is bounded
        so a permanently dead server cannot pin the thread forever.
        """
        self._disarm_x_event_watcher()
        if self._x_reconnect_thread is not None and self._x_reconnect_thread.is_alive():
            return
        old = self.xdisplay
        self.xdisplay = None
        self.keyboard = None
        self.mouse = None

        def _attempt():
            try:
                if old is not None:
                    try:
                        old.close()
                    except Exception:
                        pass
                disp = display.Display(blocking_timeout=INPUT_X_REPLY_TIMEOUT_S)
            except Exception as e:
                logger_webrtc_input.error(f"Could not reconnect input X display: {e}")
                return
            try:
                self.loop.call_soon_threadsafe(self._install_reconnected_xdisplay, disp)
            except RuntimeError:
                try:
                    disp.close()
                except Exception:
                    pass

        self._x_reconnect_thread = threading.Thread(
            target=_attempt, name="x-input-reconnect", daemon=True
        )
        self._x_reconnect_thread.start()

    def _install_reconnected_xdisplay(self, disp: Any) -> None:
        """Loop-side half of _reconnect_xdisplay: wire the fresh connection into
        every consumer in one step so nothing observes a half-initialized display."""
        if self.xdisplay is not None:
            # An earlier attempt already landed; drop this connection.
            try:
                disp.close()
            except Exception:
                pass
            return
        self.xdisplay = disp
        self._apply_input_x_reply_bound()
        self._arm_x_event_watcher()
        self.__keyboard_connect()
        if not self.is_wayland:
            self.mouse = _XTestMouse(self.xdisplay)
        if self.cursors_running:
            try:
                screen = self.xdisplay.screen()
                self.xdisplay.xfixes_select_cursor_input(
                    screen.root, xfixes.XFixesDisplayCursorNotifyMask
                )
            except Exception as e:
                logger_webrtc_input.warning(f"Could not re-arm cursor monitor after reconnect: {e}")
        logger_webrtc_input.warning("Input X connection was unresponsive; reconnected.")

    def _is_x_conn_closed(self, exc: BaseException) -> bool:
        """True if `exc` is the connection-closed error the reply bound raises."""
        return xlib_error is not None and isinstance(exc, xlib_error.ConnectionClosedError)

    async def _load_server_autorepeat_rate(self) -> None:
        """Best-effort: adopt the X server's configured autorepeat delay/rate for our
        synthetic server-side repeat, so a held key feels native. python-xlib in some
        builds lacks the XKB controls API, so read it once at connect from `xset q`
        (one-time, not per-event). Any failure or out-of-range value keeps the sane
        defaults set in __init__."""
        if self.is_wayland:
            return
        try:
            process = await subprocess.create_subprocess_exec(
                "xset", "q", stdout=subprocess.PIPE, stderr=subprocess.PIPE
            )
            out, _err = await self._communicate_or_kill(process, 0.5, "xset q autorepeat")
            if process.returncode != 0 or not out:
                return
            text = out.decode("utf-8", "replace") if isinstance(out, (bytes, bytearray)) else str(out)
            m = re.search(r"auto repeat delay:\s*(\d+)\s*repeat rate:\s*(\d+)", text)
            if not m:
                return
            delay_ms = int(m.group(1))
            rate_hz = int(m.group(2))
            # Reject nonsense (a 0) that would busy-repeat or never repeat.
            if 100 <= delay_ms <= 2000:
                self.key_repeat_delay = delay_ms / 1000.0
            if 1 <= rate_hz <= 100:
                self.key_repeat_interval = 1.0 / rate_hz
            logger_webrtc_input.info(
                f"Server autorepeat: delay {self.key_repeat_delay:.3f}s, "
                f"interval {self.key_repeat_interval:.3f}s."
            )
        except Exception as e:
            logger_webrtc_input.debug(f"Could not read server autorepeat rate (using defaults): {e}")
    def __mouse_connect(self) -> None:
        if self.uinput_mouse_socket_path:
            logger_webrtc_input.info(f"Connecting to uinput mouse socket: {self.uinput_mouse_socket_path}")
            self.uinput_mouse_socket = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
        if not self.is_wayland and self.xdisplay:
            self.mouse = _XTestMouse(self.xdisplay)
    def __mouse_disconnect(self) -> None:
        if self.mouse: del self.mouse; self.mouse = None
        if self.uinput_mouse_socket is not None:
            try:
                self.uinput_mouse_socket.close()
            except OSError:
                pass
            self.uinput_mouse_socket = None
    def __mouse_emit(self, *args: Any, **kwargs: Any) -> None:
        """Forward one msgpack-encoded mouse event to the uinput helper socket."""
        if self.uinput_mouse_socket_path:
            cmd = {"args": args, "kwargs": kwargs}
            data = msgpack.packb(cmd, use_bin_type=True)
            self.uinput_mouse_socket.sendto(data, self.uinput_mouse_socket_path)

    async def __gamepad_connect(self, gamepad_idx: int, client_name: str,
                                client_num_btns: int, client_num_axes: int,
                                conn_id: Any = None) -> None:
        """Associate a client controller with a persistent gamepad slot.

        A fresh association starts with no heartbeat, so the previous client's
        last beat cannot date-stamp it into an immediate sweep, and the kernel
        device is brought up before the first input so applications see a
        plug event rather than a controller appearing mid-press.
        """
        if not (0 <= gamepad_idx < self.num_gamepads):
            logger_webrtc_input.error(f"Client association: Gamepad index {gamepad_idx} out of range (0-{self.num_gamepads-1}).")
            return

        if gamepad_idx not in self.gamepad_instances:
            logger_webrtc_input.error(
                f"Client association: No persistent gamepad instance found for index {gamepad_idx}. "
                f"This should not happen if _initialize_persistent_gamepads ran correctly."
            )
            return

        logger_webrtc_input.info(
            f"Client controller '{client_name}' ({client_num_btns}b, {client_num_axes}a) "
            f"is now associated with persistent virtual gamepad slot {gamepad_idx}."
        )

        self.client_gamepad_associations[gamepad_idx] = {
            "client_name": client_name,
            "client_num_btns": client_num_btns,
            "client_num_axes": client_num_axes,
            "association_time": time.time(),
            "conn_id": conn_id,
        }

        self.gamepad_heartbeats.pop(gamepad_idx, None)

        self.gamepad_instances[gamepad_idx].ensure_uinput()

    async def release_gamepads_for_conn(self, conn_id: Any) -> None:
        """Disassociate (and neutralize, via reset_state) every gamepad slot whose
        association was made by this transport connection. This is the ungraceful
        path — a tab that dies mid-press never sends 'js,d', and only the transport
        knows the connection is gone."""
        if conn_id is None:
            return
        for idx, info in list(self.client_gamepad_associations.items()):
            if info.get("conn_id") == conn_id:
                await self.__gamepad_disconnect(idx)

    async def __gamepad_disconnect(self, gamepad_idx: Optional[int] = None) -> None:
        """Disassociate one slot (or all, with None), releasing anything held.

        The release matters for an ungraceful client drop (a tab closed
        mid-press), which would otherwise leave the in-desktop app with a
        stuck button or deflected stick until a new client re-sends state.
        """
        if gamepad_idx is None:
            indices_to_disassociate = list(self.client_gamepad_associations.keys())
            logger_webrtc_input.info("Disassociating all client gamepads from persistent slots.")
        elif not (0 <= gamepad_idx < self.num_gamepads):
            logger_webrtc_input.error(f"Client disassociation: Gamepad index {gamepad_idx} out of range.")
            return
        else:
            indices_to_disassociate = [gamepad_idx]

        for idx in indices_to_disassociate:
            self.gamepad_heartbeats.pop(idx, None)
            if idx in self.client_gamepad_associations:
                associated_info = self.client_gamepad_associations.pop(idx)
                gamepad = self.gamepad_instances.get(idx)
                if gamepad is not None:
                    gamepad.reset_state()
                logger_webrtc_input.info(
                    f"Client controller '{associated_info.get('client_name', 'Unknown')}' "
                    f"disassociated from persistent virtual gamepad slot {idx}."
                )
            elif gamepad_idx is not None:
                 logger_webrtc_input.warning(
                    f"Client disassociation: No active client association found for gamepad slot {idx} to disassociate."
                )

    def __gamepad_emit_btn(self, gamepad_idx: int, client_btn_num: int,
                           client_btn_val: float) -> None:
        gamepad = self.gamepad_instances.get(gamepad_idx)
        if gamepad:
            gamepad.send_event(client_btn_num, client_btn_val, is_button_event=True)

    def __gamepad_emit_axis(self, gamepad_idx: int, client_axis_num: int,
                            client_axis_val: float) -> None:
        gamepad = self.gamepad_instances.get(gamepad_idx)
        if gamepad:
            gamepad.send_event(client_axis_num, client_axis_val, is_button_event=False)
            
    async def connect(self) -> None:
        """Bring the input backends up: X/Wayland connections, DPI detection,
        keyboard reset, persistent gamepads, and the background key tasks."""
        if not self.is_wayland and X11_LIBS_AVAILABLE:
            # Bounded handshake: a server hung under another client's grab must
            # surface as a failure, not freeze the loop.
            try: self.xdisplay = display.Display(blocking_timeout=INPUT_X_REPLY_TIMEOUT_S)
            except Exception as e: logger_webrtc_input.error(f"Failed to connect to X display: {e}"); self.xdisplay = None
            self._apply_input_x_reply_bound()
            if self._x_event_wake is None:
                self._x_event_wake = asyncio.Event()
            self._arm_x_event_watcher()
        if self.xdisplay:
            try:
                screen = self.xdisplay.screen()
                width_mm = screen.width_in_mms
                height_mm = screen.height_in_mms
                if width_mm > 0 and height_mm > 0:
                    dpi_x = (screen.width_in_pixels * 25.4) / width_mm
                    dpi_y = (screen.height_in_pixels * 25.4) / height_mm
                    self.system_dpi = (dpi_x + dpi_y) / 2.0
                dpi_scale_factor = self.system_dpi / 96.0
                self.cursor_size_cap = int(self.max_cursor_size * dpi_scale_factor)
                logger_webrtc_input.info(
                    f"System DPI detected as ~{self.system_dpi:.0f}. "
                    f"Cursor size cap set to {self.cursor_size_cap}x{self.cursor_size_cap}px."
                )
            except Exception as e:
                logger_webrtc_input.warning(f"Could not determine system DPI, using default 96. Error: {e}")
        if not self.is_wayland and X11_LIBS_AVAILABLE:
            self.__keyboard_connect()
        if self.xdisplay:
            await self._load_server_autorepeat_rate()
            await self.reset_keyboard()
        self.__mouse_connect()
        
        await self._initialize_persistent_gamepads()

        if self.is_wayland:
            self.keyboard_worker_task = asyncio.create_task(self._keyboard_worker())
            await self._push_wayland_base_layout()
            # After the worker starts: a Lock left engaged by a previous session
            # inverts every letter, and the reset rides the fresh worker.
            await self.reset_keyboard()
        if self.key_sweep_task is None:
            self.key_sweep_task = asyncio.create_task(self._key_stale_sweep())
        if self.key_repeat_enabled and self.key_repeat_task is None:
            self.key_repeat_task = asyncio.create_task(self._key_repeat_loop())
        if self.xdisplay is not None and self.keymap_watch_task is None:
            self.keymap_watch_task = asyncio.create_task(self._keymap_watch_loop())

    async def _initialize_persistent_gamepads(self) -> None:
        """Adopt live process-wide gamepad instances or create and start new ones.

        A live instance's sockets are what already-running apps hold open, so
        a service restart (transport mode switch) reuses them; rebinding would
        orphan those apps' fds.
        """
        logger_webrtc_input.info(f"Initializing {self.num_gamepads} persistent gamepad instances...")
        if not os.path.exists(self.js_socket_path_prefix):
            try:
                os.makedirs(self.js_socket_path_prefix, exist_ok=True)
                logger_webrtc_input.info(f"Created directory for gamepad sockets: {self.js_socket_path_prefix}")
            except OSError as e:
                logger_webrtc_input.error(f"Failed to create directory {self.js_socket_path_prefix} for gamepad sockets: {e}")
                return

        for i in range(self.num_gamepads):
            if i in self.gamepad_instances:
                logger_webrtc_input.warning(f"Gamepad instance for index {i} already exists. Skipping re-initialization.")
                continue

            existing = _persistent_gamepads.get(i)
            if existing is not None and existing.running:
                self.gamepad_instances[i] = existing
                logger_webrtc_input.info(
                    f"Adopted live persistent gamepad instance for index {i} (JS: {existing.js_sock_path})."
                )
                continue

            js_ip_sock_path = os.path.join(self.js_socket_path_prefix, f"selkies_js{i}.sock")
            evdev_ip_sock_path = os.path.join(self.js_socket_path_prefix, f"selkies_event{1000+i}.sock")

            gamepad = SelkiesGamepad(
                js_ip_sock_path, evdev_ip_sock_path, self.loop,
                uinput_enabled=self.uinput_gamepads,
            )

            gamepad_name_for_interposer = STANDARD_XPAD_CONFIG.get("name", f"Selkies Virtual Gamepad {i}")
            std_num_btns = len(STANDARD_XPAD_CONFIG["btn_map"])
            std_num_axes = len(STANDARD_XPAD_CONFIG["axes_map"])

            gamepad.set_config(gamepad_name_for_interposer, std_num_btns, std_num_axes)

            self._spawn_task(gamepad.run_servers())
            _persistent_gamepads[i] = gamepad
            self.gamepad_instances[i] = gamepad
            logger_webrtc_input.info(f"Initialized and started persistent gamepad instance for index {i} (Name: '{gamepad_name_for_interposer}', JS: {js_ip_sock_path}, EVDEV: {evdev_ip_sock_path}).")

    async def disconnect(self) -> None:
        """Tear down this handler's own resources; persistent gamepads stay up.

        Only the per-session client-to-slot associations are this handler's
        (see SelkiesGamepad). The input X connection is closed, not just
        dropped: the keyboard shim holds the same Display and python-xlib's
        root-window back-reference makes the graph a cycle with no finalizer,
        so a dropped connection would keep its X client slot until a cyclic
        collection happens to run — and every transport switch builds a new
        handler.
        """
        logger_webrtc_input.info("Releasing gamepad associations (persistent instances stay up).")
        await self.__gamepad_disconnect()
        self.gamepad_instances = {}
        self.gamepad_heartbeats.clear()
        # Before the pointer backends go away, or a held button stays pressed for good.
        await self.release_mouse_buttons()
        self.__mouse_disconnect()
        self._disarm_x_event_watcher()
        if self.keyboard is not None:
            self.keyboard.release_group_lock()
        old_display, self.xdisplay, self.keyboard = self.xdisplay, None, None
        if old_display is not None:
            try:
                await asyncio.to_thread(old_display.close)
            except Exception as e:
                logger_webrtc_input.debug(f"closing the input X connection failed: {e}")

        if self.keyboard_worker_task:
            self.keyboard_worker_task.cancel()
            self.keyboard_worker_task = None
        if self.key_sweep_task:
            self.key_sweep_task.cancel()
            self.key_sweep_task = None
        if self.key_repeat_task:
            self.key_repeat_task.cancel()
            self.key_repeat_task = None
        if self.keymap_watch_task:
            self.keymap_watch_task.cancel()
            self.keymap_watch_task = None
        self.pressed_keys.clear()
        self.reaped_atomic_keys.clear()
        self.key_repeat_state.clear()
        self._reset_multipart_clipboard()

    async def _key_stale_sweep(self) -> None:
        """Auto-release keys and neutralize gamepads whose heartbeats stopped,
        so no input stays stuck held when a key-up is lost to congestion or the
        client vanishes without a transport close.

        On Wayland a release rides the serialized keyboard queue. X11
        injection is not queue-serialized, so the modifier/atomic state
        discard is deferred until after the release and a concurrent kd is
        checked for on both sides of the await: the keysym is popped first, so
        a non-None entry means a kd re-pressed it and already injected its own
        keydown, and the sweep then abandons its release (a second down would
        double-press) and leaves that kd's state intact.
        """
        try:
            while True:
                await asyncio.sleep(self.key_sweep_interval)
                if not self.pressed_keys and not self.gamepad_heartbeats:
                    continue
                now = time.monotonic()
                stale = [k for k, seen in self.pressed_keys.items() if now - seen > self.key_stale_window]
                for keysym in stale:
                    # A heartbeat or re-press during a prior await may have
                    # refreshed this key.
                    seen = self.pressed_keys.get(keysym)
                    if seen is None or time.monotonic() - seen <= self.key_stale_window:
                        continue
                    was_atomic = keysym in self.atomically_typed_keys
                    self.pressed_keys.pop(keysym, None)
                    self.key_repeat_state.pop(keysym, None)
                    logger_webrtc_input.warning(f"Auto-releasing key {keysym} (heartbeat lost).")
                    # An atomically-typed key was never physically held on X11.
                    if was_atomic and not self.is_wayland:
                        self.atomically_typed_keys.discard(keysym)
                        if len(self.reaped_atomic_keys) < self.max_pressed_keys:
                            self.reaped_atomic_keys.add(keysym)
                        continue
                    try:
                        if self.is_wayland:
                            self.active_modifiers.discard(keysym)
                            self.atomically_typed_keys.discard(keysym)
                            self._keyboard_enqueue(("ku", keysym))
                        else:
                            if self.pressed_keys.get(keysym) is not None:
                                # A kd raced us: it owns the key now.
                                continue
                            await self.send_x11_keypress(keysym, down=False)
                            if self.pressed_keys.get(keysym) is not None:
                                # A kd raced the keyup await: leave its state intact.
                                continue
                            self.active_modifiers.discard(keysym)
                            self.atomically_typed_keys.discard(keysym)
                    except Exception as e:
                        logger_webrtc_input.warning(f"Failed to auto-release key {keysym}: {e}")
                for idx, seen in list(self.gamepad_heartbeats.items()):
                    if now - seen <= self.key_stale_window:
                        continue
                    self.gamepad_heartbeats.pop(idx, None)
                    gamepad = self.gamepad_instances.get(idx)
                    if gamepad is not None and gamepad._held_controls:
                        logger_webrtc_input.warning(
                            f"Neutralizing gamepad slot {idx} (heartbeat lost).")
                        gamepad.reset_state()
        except asyncio.CancelledError:
            pass

    async def _key_repeat_loop(self) -> None:
        """X11 server-side key auto-repeat for held keys.

        XTEST/xdotool synthetic presses don't trigger the X server's native
        auto-repeat, so without this a held key types a single character. We re-emit the
        most-recently-pressed still-held repeatable key at key_repeat_interval after an
        initial key_repeat_delay -- exactly like a physical keyboard, where only the last
        key pressed repeats and releasing it resumes the previously-held one. Modifiers
        are never armed, so the repeated key carries whatever modifiers are currently
        held (Shift+Arrow selection, Ctrl+Backspace word-delete, Ctrl+Z, etc. all repeat
        like native -- no special shortcut suppression); atomically-typed keys
        (digits/punctuation) are armed and repeat through the atomic path below.
        Repeats are KeyPress-only (no synthetic KeyRelease),
        matching X11 detectable auto-repeat, so state-based games keep the key held with
        no movement stutter and ignore the extra presses. Wayland is excluded (the
        focused app repeats held virtual-keyboard keys itself via wl_keyboard
        repeat_info, so a server-side repeat would double it).

        key_repeat_state is insertion-ordered and arming moves a key to the
        end, so its last entry is the newest held key. An atomic key repeats
        as a self-contained XTEST press+release at its shift level (the 'ku'
        path injects no key-up for atomic keys, so a lone press would stick),
        falling to the co,end path when it has no keycode in the layout.
        Repeat pauses while the key's heartbeats have stopped (stalled stream,
        hidden tab), bounding run-on to the grace rather than the stale window.
        """
        try:
            while True:
                await asyncio.sleep(self.key_repeat_tick)
                if not self.key_repeat_state:
                    continue
                # Keys released/reaped since arming: never inject a down after the key-up.
                for k in [k for k in self.key_repeat_state if k not in self.pressed_keys]:
                    self.key_repeat_state.pop(k, None)
                if not self.key_repeat_state:
                    continue
                keysym = next(reversed(self.key_repeat_state))
                now = time.monotonic()
                if now < self.key_repeat_state[keysym]:
                    continue
                last_seen = self.pressed_keys.get(keysym)
                if last_seen is None or (now - last_seen) > self.key_repeat_heartbeat_grace:
                    continue
                try:
                    if keysym in self.atomically_typed_keys:
                        injected = False
                        if self.keyboard is not None:
                            try:
                                self.keyboard.press(
                                    keysym,
                                    held_keysyms=(self.active_modifiers
                                                  & self.LEVEL_MODIFIER_KEYSYMS))
                                self.keyboard.release(keysym)
                                injected = True
                            except Exception as e:
                                logger_webrtc_input.debug(
                                    f"XTEST atomic repeat failed for keysym {keysym}; falling back: {e}"
                                )
                                if self._is_x_conn_closed(e):
                                    self._reconnect_xdisplay()
                        if not injected:
                            unicode_codepoint = (keysym & 0x00FFFFFF
                                                 if (keysym & 0xFF000000) == 0x01000000 else keysym)
                            char_to_type = chr(unicode_codepoint)
                            await self.on_message(f"co,end,{char_to_type}")
                    else:
                        await self.send_x11_keypress(keysym, down=True)
                except Exception as e:
                    logger_webrtc_input.warning(f"Key auto-repeat failed for {keysym}: {e}")
                    self.key_repeat_state.pop(keysym, None)
                    continue
                # A 'ku' during the await released the key; the extra down is
                # healed by the real key-up / stale sweep.
                if keysym in self.pressed_keys:
                    self.key_repeat_state[keysym] = time.monotonic() + self.key_repeat_interval
                else:
                    self.key_repeat_state.pop(keysym, None)
        except asyncio.CancelledError:
            pass

    # Bound on waiting for a queued Wayland reset: the worker drains key work
    # in client order, each item a bounded compositor round-trip at most.
    _WL_RESET_WAIT_S = 5.0

    async def reset_keyboard(self) -> None:
        """Release every held key/modifier and normalize a stuck Caps Lock.

        Runs on client 'kr' (blur/visibility loss), at connect, and when a
        client holding input departs, on both backends: the client resolves
        letter case itself, so an engaged Lock modifier on the server would
        invert every letter it types. On Wayland every key goes through the
        serialized keyboard worker, so the reset is queued behind the key work
        in flight like a client 'kr' is (a press still queued behind a text
        batch would otherwise land after the reset, untracked and held for
        good); it runs in place only when no worker is draining the queue.

        On X11 every still-held key is released, not just the modifier and
        hotkey list: once pressed_keys is cleared the stale sweep (the only
        other releaser) never runs for them, so a held 'w' in a game would
        stay pressed forever. Atomically-typed keys were never physically
        held and are skipped to avoid a spurious keyup; translated (Cyrillic)
        keys go through send_x11_keypress's translation so the injected QWERTY
        key comes up. Caps Lock is not forwarded from the browser, but a prior
        session or the desktop's own startup can leave Lock engaged, so it is
        toggled back off.
        """
        if self.is_wayland:
            worker = self.keyboard_worker_task
            if (worker is None or worker.done()
                    or asyncio.current_task() is worker):
                await self._reset_keyboard_wayland()
                return
            done = asyncio.get_running_loop().create_future()
            self._keyboard_enqueue(("kr", done))
            await asyncio.wait({done, worker}, timeout=self._WL_RESET_WAIT_S,
                               return_when=asyncio.FIRST_COMPLETED)
            if done.done():
                return
            if worker.done():
                # The worker died with the reset queued; nothing else injects now.
                await self._reset_keyboard_wayland()
            else:
                logger_webrtc_input.warning(
                    "Keyboard reset still queued behind pending key work.")
            return

        if not self.keyboard or not self.xdisplay :
            logger_webrtc_input.warning("Cannot reset keyboard, X display or keyboard controller not available.")
            return
        logger_webrtc_input.info("Resetting keyboard modifiers.")
        lctrl, lshift, lalt, altgr = 65507, 65505, 65513, 65027
        rctrl, rshift, ralt = 65508, 65506, 65514
        lmeta, rmeta, keyf, keyF, keym, keyM, escape = 65511, 65512, 102, 70, 109, 77, 65307
        # Super/Hyper included: the client maps the Meta/Windows key to Super.
        lsuper, rsuper, lhyper, rhyper = 65515, 65516, 65517, 65518
        for k in [lctrl, lshift, lalt, altgr, rctrl, rshift, ralt, lmeta, rmeta,
                  lsuper, rsuper, lhyper, rhyper, keyf, keyF, keym, keyM, escape]:
            try: await self.send_x11_keypress(k, down=False)
            except Exception as e: logger_webrtc_input.warning(f"Error resetting key {k}: {e}")
        for keysym in list(self.pressed_keys):
            if keysym in self.atomically_typed_keys:
                continue
            try: await self.send_x11_keypress(keysym, down=False)
            except Exception as e: logger_webrtc_input.warning(f"Error releasing held key {keysym}: {e}")
        for k in list(self.translated_keys):
            try: await self.send_x11_keypress(k, down=False)
            except Exception as e: logger_webrtc_input.warning(f"Error releasing translated key {k}: {e}")
        try:
            if self.xdisplay.screen().root.query_pointer().mask & Xlib.X.LockMask:
                caps_kc = self.xdisplay.keysym_to_keycode(0xffe5)
                if caps_kc:
                    xtest.fake_input(self.xdisplay, Xlib.X.KeyPress, caps_kc)
                    xtest.fake_input(self.xdisplay, Xlib.X.KeyRelease, caps_kc)
                    self.xdisplay.flush()
        except Exception as e:
            logger_webrtc_input.warning(f"Could not normalize Lock modifier: {e}")
        # Every key is up, so a group lock of ours goes back at once, not after its linger.
        if self.keyboard is not None:
            self.keyboard.release_group_lock()
        # After the release loops, which consume this state.
        self.active_modifiers.clear()
        self.active_shortcut_modifiers.clear()
        self.atomically_typed_keys.clear()
        self.translated_keys.clear()
        # Or the stale sweep would auto-release keys the reset just cleared.
        self.pressed_keys.clear()
        self.reaped_atomic_keys.clear()
        self.key_repeat_state.clear()

    async def _reset_keyboard_wayland(self) -> None:
        """The Wayland reset itself — see reset_keyboard for when it runs and
        why every held and translated key is released before the maps clear.

        Caps Lock is read back through get_keyboard_state — a compositor
        round-trip answered by the thread that also renders, so it waits off
        the loop — and toggled off with a virtual Caps_Lock press+release.
        """
        if self.wayland_input:
            # Ctrl, Shift, Alt, AltGr, Meta, Super, Hyper (the client maps the
            # Meta/Windows key to Super).
            modifiers = [65507, 65505, 65513, 65508, 65506, 65514, 65027,
                         65511, 65512, 65515, 65516, 65517, 65518]
            # A stale owner (rebuild backing off) still knows what is down.
            owner = await self._ensure_wayland_keymap_owner() or self._wl_keymap_owner
            if owner is not None:
                for k in modifiers:
                    try:
                        owner.release(k)
                    except Exception:
                        pass
                try:
                    owner.reset()
                except Exception:
                    pass
        for k in list(self.translated_keys):
            try: await self.send_x11_keypress(k, down=False)
            except Exception: pass
        for keysym in list(self.pressed_keys):
            if keysym in self.atomically_typed_keys:
                continue
            try: await self.send_x11_keypress(keysym, down=False)
            except Exception as e: logger_webrtc_input.warning(f"Error releasing held key {keysym}: {e}")
        self.active_modifiers.clear()
        self.active_shortcut_modifiers.clear()
        self.atomically_typed_keys.clear()
        self.translated_keys.clear()
        # Or the stale sweep would auto-release keys the reset just cleared.
        self.pressed_keys.clear()
        self.reaped_atomic_keys.clear()
        try:
            if self.wayland_input and hasattr(self.wayland_input, 'get_keyboard_state'):
                _pressed, mods = await asyncio.to_thread(self.wayland_input.get_keyboard_state)
                # mods bit 4 is caps_lock.
                if mods & 0x10:
                    keymap_owner = await self._ensure_wayland_keymap_owner()
                    if keymap_owner is not None:
                        keymap_owner.press(0xffe5)
                        await asyncio.sleep(0.02)
                        keymap_owner.release(0xffe5)
        except Exception as e:
            logger_webrtc_input.debug(f"Wayland caps-lock normalization skipped: {e}")

    async def release_mouse_buttons(self) -> None:
        """Release every pointer button still held server-side.

        The ungraceful pointer path, mirroring release_gamepads_for_conn and the
        key stale-sweep: a client that dies mid-drag never sends the mask with the
        button cleared, so the desktop would keep the drag or selection alive until
        some future client happens to diff the bit away. The releases ride the
        normal mask-diff loop, so X11 and Wayland both heal; the zero-delta
        relative move leaves the pointer exactly where it is (and injects no motion
        at all on Wayland)."""
        if not self.button_mask:
            return
        try:
            await self.send_x11_mouse(0, 0, 0, 0, relative=True)
        except Exception as e:
            logger_webrtc_input.warning(f"Failed to release held mouse buttons: {e}")

    def send_mouse(self, action: int, data: Any) -> None:
        """Route one MOUSE_* action to the uinput socket or XTEST backend.

        Args:
            action: A MOUSE_* constant.
            data: Action-dependent — an (x, y) pair for position/move, a
                (press/release, button-id) pair for MOUSE_BUTTON, None for
                scroll actions.
        """
        if action == MOUSE_POSITION:
            if self.mouse: self.mouse.position = data
        elif action == MOUSE_MOVE:
            x, y = data
            if self.uinput_mouse_socket_path:
                self.__mouse_emit(UINPUT_REL_X, x, syn=False)
                self.__mouse_emit(UINPUT_REL_Y, y)
            elif self.xdisplay:
                xtest.fake_input(self.xdisplay, Xlib.X.MotionNotify, detail=True, root=Xlib.X.NONE, x=x, y=y)
                # flush(), not sync(): XTEST needs no reply and sync() is a round trip per move.
                self.xdisplay.flush()
        elif action == MOUSE_SCROLL_UP:
            # MOUSE_SCROLL_* are named for the client button, not the physical
            # direction: this is wheel-down (X button 5), so REL_WHEEL is -1.
            if self.uinput_mouse_socket_path: self.__mouse_emit(UINPUT_REL_WHEEL, -1)
            elif self.mouse: self.mouse.scroll(0, -1)
        elif action == MOUSE_SCROLL_DOWN:
            if self.uinput_mouse_socket_path: self.__mouse_emit(UINPUT_REL_WHEEL, 1)
            elif self.mouse: self.mouse.scroll(0, 1)
        elif action == MOUSE_SCROLL_LEFT:
            # REL_HWHEEL is signed as the client names it (negative = left), like
            # X buttons 6/7, so no flip here.
            if self.uinput_mouse_socket_path: self.__mouse_emit(UINPUT_REL_HWHEEL, -1)
            elif self.mouse: self.mouse.scroll(-1, 0)
        elif action == MOUSE_SCROLL_RIGHT:
            if self.uinput_mouse_socket_path: self.__mouse_emit(UINPUT_REL_HWHEEL, 1)
            elif self.mouse: self.mouse.scroll(1, 0)
        elif action == MOUSE_BUTTON: 
            btn_map_key = "uinput" if self.uinput_mouse_socket_path else "x11"
            btn_uinput_or_x11 = MOUSE_BUTTON_MAP[data[1]][btn_map_key]
            if data[0] == MOUSE_BUTTON_PRESS:
                if self.uinput_mouse_socket_path: self.__mouse_emit(btn_uinput_or_x11, 1)
                elif self.mouse: self.mouse.press(btn_uinput_or_x11)
            else:
                if self.uinput_mouse_socket_path: self.__mouse_emit(btn_uinput_or_x11, 0)
                elif self.mouse: self.mouse.release(btn_uinput_or_x11)

    async def send_x11_keypress(self, keysym: int, down: bool = True,
                                neutralize: Optional[bool] = None) -> None:
        """Inject one key transition on whichever backend this session uses.

        Despite the name this is the shared key injector: Wayland routes
        through the seat keymap owner (virtual-keyboard/clipboard on error),
        X11 through XTEST with xdotool fallbacks. Cyrillic keysyms chorded
        with an action modifier are translated to the QWERTY keysym on the
        same physical key so shortcuts (Ctrl+C on a ЙЦУКЕН layout) reach the
        application as the app expects.

        On X11 a keysym with a keycode in the current keymap is injected
        through XTEST on the already-open display, which spares a ~15 ms
        xdotool fork per shortcut, arrow or function key; a keysym the layout
        lacks is overlay-bound once by the shim and reused, and one only a
        later layout group carries is injected under that group's lock —
        never a per-key xdotool fork, whose transient rebind floods
        MappingNotify and lags the whole input queue behind real typing.
        xdotool remains for a keysym with no keycode at all that it can still
        synthesize.

        Args:
            neutralize: Whether a conflicting held Shift/AltGr is lifted around
                the key. None derives it from the client's held modifiers:
                lifted around plain keystrokes only, since while a chord
                modifier (Ctrl/Alt/Super/...) is down every held modifier is
                part of the chord. A server-synthesized chord passes False,
                since a Shift it pressed through this injector is not in
                active_modifiers and would be lifted for the very key it
                modifies.
        """
        if down:
            if (self.active_modifiers & self.ACTION_MODIFIER_KEYSYMS) and keysym in CYRILLIC_TO_QWERTY_KEYSYM:
                self.translated_keys.add(keysym)
                keysym = CYRILLIC_TO_QWERTY_KEYSYM[keysym]
        else:
            if keysym in self.translated_keys:
                self.translated_keys.discard(keysym)
                keysym = CYRILLIC_TO_QWERTY_KEYSYM[keysym]

        if neutralize is None:
            neutralize = (keysym not in self.MODIFIER_KEYSYMS
                          and not (self.active_modifiers & self.ACTION_MODIFIER_KEYSYMS))
        held_level_mods = frozenset(self.active_modifiers & self.LEVEL_MODIFIER_KEYSYMS)

        if self.is_wayland and self.wayland_input:
            owner = await self._ensure_wayland_keymap_owner()
            if owner is not None:
                try:
                    if down:
                        owner.press(keysym, neutralize=neutralize)
                    else:
                        owner.release(keysym)
                    return
                except Exception as e:
                    logger_webrtc_input.warning(
                        f"Wayland keymap injection failed for keysym {keysym}; falling back: {e}"
                    )
            await self._type_keysym_fallback(keysym, down)
            return

        is_printable = (0x20 <= keysym <= 0xFF) or ((keysym & 0xFF000000) == 0x01000000)
        action = "keydown" if down else "keyup"
        command = None
        use_keyboard_for_printable = False
        allow_xtest = False
        if is_printable:
            unicode_codepoint = keysym & 0x00FFFFFF if (keysym & 0xFF000000) == 0x01000000 else keysym
            try:
                char = chr(unicode_codepoint)
                if char.isalpha():
                    use_keyboard_for_printable = True
                else:
                    xdotool_arg = f"U{unicode_codepoint:04X}"
                    if not self.active_shortcut_modifiers:
                        use_keyboard_for_printable = True
                    else:
                        command = ["xdotool", action, xdotool_arg]
                        allow_xtest = True
            except ValueError:
                use_keyboard_for_printable = True

        else:
            map_entry = X11_KEYSYM_MAP.get(keysym)
            if map_entry:
                xdotool_arg = map_entry.get('xkey_name')
                if xdotool_arg:
                    command = ["xdotool", action, xdotool_arg]
                    allow_xtest = True
                    if xdotool_arg in self.SHORTCUT_MODIFIER_XKEY_NAMES:
                        if down:
                            self.active_shortcut_modifiers.add(xdotool_arg)
                        else:
                            self.active_shortcut_modifiers.discard(xdotool_arg)

        if command:
            if allow_xtest and xtest is not None and self.xdisplay is not None:
                try:
                    keycode = self.xdisplay.keysym_to_keycode(keysym)
                    if self.keyboard and self.keyboard.outside_base_group(keysym):
                        # A bare keycode would type the group-1 glyph; the shim
                        # locks the group around the injection instead.
                        keycode = 0
                    if keycode:
                        xtest.fake_input(
                            self.xdisplay,
                            X.KeyPress if down else X.KeyRelease,
                            keycode,
                        )
                        self.xdisplay.flush()
                        return
                    if self.keyboard:
                        if down:
                            self.keyboard.press(keysym, neutralize=neutralize,
                                                held_keysyms=held_level_mods)
                        else:
                            self.keyboard.release(keysym)
                        return
                except Exception as e:
                    logger_webrtc_input.debug(
                        f"XTEST inject failed for keysym {keysym}; falling back to xdotool: {e}"
                    )
            try:
                process = await subprocess.create_subprocess_exec(
                    *command, stdout=subprocess.PIPE, stderr=subprocess.PIPE
                )
                await self._communicate_or_kill(process, 0.5, "xdotool key")
                if process.returncode == 0:
                    return
                logger_webrtc_input.warning(
                    f"xdotool {action} failed (rc={process.returncode}) for keysym {keysym}")
            except Exception:
                pass

        if use_keyboard_for_printable or not command:
            try:
                if not self.keyboard:
                    await self._type_keysym_fallback(keysym, down)
                    return

                if down:
                    self.keyboard.press(keysym, neutralize=neutralize,
                                        held_keysyms=held_level_mods)
                else:
                    self.keyboard.release(keysym)
            except Exception as e:
                if self._is_x_conn_closed(e):
                    self._reconnect_xdisplay()
                await self._type_keysym_fallback(keysym, down)

    def _type_text_xtest(self, text: str, neutralize: bool = False) -> bool:
        """Type a string in-process via the XTEST shim.

        Each char is a press+release of its keysym (mapped chars with shift
        synthesis, unmapped ones via the spare-keycode overlay). A char prefers
        its canonical layout keysym, as the Wayland owner's type_text does, so
        a layout that carries the script types on its own keys (Cyrillic on
        ru, or on us,ru under a group lock) instead of spending overlay slots
        on every letter. With neutralize, conflicting held Shift/AltGr are
        lifted around the whole run (one keymap query, not one per char).
        Unmapped chars are bound in one batch (O(1) MappingNotify broadcasts
        instead of one per char), and nothing is typed on failure.

        Returns:
            True on full success; False (having typed nothing) if the shim is
            unavailable or any char can't be resolved, so the caller can fall
            back to xdotool without double-typing.
        """
        if not self.keyboard or not text:
            return False
        # Pre-resolve every char so a mid-string failure types no partial line.
        keysyms = []
        for ch in text:
            ks = character_to_layout_keysym(ch)
            if not self.keyboard.layout_carries(ks):
                cp = ord(ch)
                ks = cp if 0x20 <= cp <= 0xFF else (0x01000000 | cp)
            keysyms.append(ks)
        try:
            if not self.keyboard.prebind(keysyms):
                return False
            lifted = []
            if neutralize:
                down = self.keyboard._down_mod_keycodes(
                    self.active_modifiers & self.LEVEL_MODIFIER_KEYSYMS)
                lifted = self.keyboard._mods_to_lift(set(), down)
            for m in lifted:
                xtest.fake_input(self.keyboard._d, Xlib.X.KeyRelease, m)
            try:
                for ks in keysyms:
                    self.keyboard.press(ks)
                    self.keyboard.release(ks)
            finally:
                for m in reversed(lifted):
                    xtest.fake_input(self.keyboard._d, Xlib.X.KeyPress, m)
                if lifted:
                    self.keyboard._d.flush()
            return True
        except Exception as e:
            logger_webrtc_input.debug(f"in-process type failed ({e}); falling back to xdotool")
            return False


    def _spawn_task(self, coro: Any, name: Optional[str] = None) -> asyncio.Task:
        """create_task with a keep-alive reference and error logging."""
        task = asyncio.create_task(coro, name=name)
        self._bg_tasks.add(task)

        def _done(t):
            self._bg_tasks.discard(t)
            if not t.cancelled() and t.exception() is not None:
                logger_webrtc_input.error(f"Background task {t.get_name()} failed: {t.exception()}")

        task.add_done_callback(_done)
        return task

    def _invalidate_wayland_keymap_owner(self) -> None:
        """The seat's base layout changed: the owner's keycode resolution is
        against the old base, so it is rebuilt — on the keyboard worker, where
        nothing injects concurrently — carrying what it holds down."""
        self._wl_keymap_stale = True
        self._wl_keymap_retry_at = 0.0

    async def _push_wayland_base_layout(self, restore: bool = False) -> None:
        """Set the compositor seat's BASE xkb layout from the deployment's XKB
        env config (XKB_DEFAULT_LAYOUT et al.): at session start, so common
        non-US keysyms resolve as base keys instead of overlay binds, and again
        (`restore`) when a nested session compositor turns up after a client
        hint moved the seat off that layout — the nested compositor translates
        keycodes with the keymap it built from the same env, so the seat must
        resolve keysyms against it; with no env layout the xkbcommon default is
        what both run on. The compositor re-splices its overlay binds on top
        with unchanged keycodes; the keymap owner rebuilds from the new base."""
        layout = os.environ.get("XKB_DEFAULT_LAYOUT", "")
        setter = getattr(self.wayland_input, 'set_xkb_layout', None)
        if setter is None or not (layout or restore):
            return
        variant = os.environ.get("XKB_DEFAULT_VARIANT", "")
        options = os.environ.get("XKB_DEFAULT_OPTIONS", "")
        model = os.environ.get("XKB_DEFAULT_MODEL", "")
        rules = os.environ.get("XKB_DEFAULT_RULES", "")
        try:
            ok = await asyncio.to_thread(
                setter, layout, variant, options, model, rules)
        except Exception as e:
            logger_webrtc_input.warning(f"Wayland base layout push failed: {e}")
            return
        if ok:
            self._wl_seat_client_layout = None
            self._invalidate_wayland_keymap_owner()
            logger_webrtc_input.info(
                f"Wayland base layout set to '{layout or 'default'}'"
                + (f" ({variant})" if variant else "")
                + (" (restored for the session compositor)" if restore else ""))
        else:
            logger_webrtc_input.warning(
                f"Wayland base layout '{layout}' rejected by the compositor; "
                "keeping the default.")

    def _schedule_seat_layout_restore(self) -> None:
        """A nested session compositor was just adopted while the seat carries
        a client layout hint: put the seat back on the deployment layout the
        session translates keycodes with."""
        if getattr(self, "_wl_seat_client_layout", None) is None:
            return
        try:
            asyncio.get_running_loop()
        except RuntimeError:
            return
        self._spawn_task(self._push_wayland_base_layout(restore=True))

    async def apply_client_keyboard_layout(self, layout_hint: Any) -> None:
        """Client SETTINGS 'keyboardLayout' hint ("de", "ch(fr)", ...). On
        pixelflux Wayland with the apps on the capture compositor it becomes
        the seat's BASE xkb layout (set_xkb_layout), so the client's physical
        layout resolves as base keys instead of per-keysym overlay binds.
        Under a nested session compositor it is informational, as on X11: the
        session translates keycodes with its own keymap, so a seat moved to
        the client layout transposes every key the two layouts place apart.
        Idempotent per value — clients re-assert SETTINGS on reconnects and
        broadcasts — and a hint only noted under a session compositor is
        applied once the apps are back on the capture compositor."""
        hint = str(layout_hint or "").strip()
        m = re.fullmatch(r"([A-Za-z0-9_,\- ]{1,32})(?:\(([A-Za-z0-9_,\- ]{1,32})\))?", hint)
        if not m:
            if hint:
                logger_webrtc_input.warning(
                    f"Ignoring malformed keyboardLayout hint: {hint[:48]!r}")
            return
        layout, variant = m.group(1).strip(), (m.group(2) or "").strip()
        noted = hint == self._client_kb_layout
        self._client_kb_layout = hint
        if not self.is_wayland:
            if not noted:
                logger_webrtc_input.info(
                    f"Client keyboard layout hint '{hint}' noted (X11 keymap is "
                    "deployment-owned; not applied).")
            return
        if self._has_separate_app_compositor():
            if not noted:
                logger_webrtc_input.info(
                    f"Client keyboard layout hint '{hint}' noted (the session "
                    "compositor owns the keymap; not applied).")
            return
        if hint == self._wl_seat_client_layout:
            return
        setter = getattr(self.wayland_input, 'set_xkb_layout', None) if self.wayland_input else None
        if setter is None:
            logger_webrtc_input.warning(
                f"Client keyboard layout '{hint}' not applied: compositor "
                "keymap control unavailable.")
            return
        try:
            ok = await asyncio.to_thread(setter, layout, variant, "", "", "")
        except Exception as e:
            logger_webrtc_input.warning(f"Wayland base layout push failed: {e}")
            return
        if ok:
            self._wl_seat_client_layout = hint
            self._invalidate_wayland_keymap_owner()
            logger_webrtc_input.info(
                f"Wayland base layout set to '{layout}'"
                + (f" ({variant})" if variant else "") + " from client hint")
        else:
            logger_webrtc_input.warning(
                f"Wayland base layout '{hint}' rejected by the compositor; "
                "keeping the current layout.")

    async def _ensure_wayland_keymap_owner(self) -> Optional[_WaylandKeymapOwner]:
        """Get-or-build the keymap owner; a stale one (base layout changed) is
        rebuilt the same way, adopting the keys it holds down. Reading the
        compositor keymap blocks (bounded) and compiling it costs milliseconds,
        so both run off the loop; after that, press/release are sync dict work
        + channel sends. None while a failed build backs off (callers fall to
        the next injection rung)."""
        if self._wl_keymap_owner is not None and not self._wl_keymap_stale:
            return self._wl_keymap_owner
        if not hasattr(self.wayland_input, 'set_keymap_string'):
            return None
        now = time.monotonic()
        if now < self._wl_keymap_retry_at:
            return None
        async with self._wl_keymap_owner_lock:
            if self._wl_keymap_owner is not None and not self._wl_keymap_stale:
                return self._wl_keymap_owner
            loop = asyncio.get_running_loop()
            previous = self._wl_keymap_owner
            try:
                def _build():
                    text = self.wayland_input.get_xkb_keymap_string()
                    owner = _WaylandKeymapOwner(self.wayland_input, text)
                    if previous is not None:
                        owner.adopt_held(previous)
                    return owner
                self._wl_keymap_owner = await loop.run_in_executor(None, _build)
                self._wl_keymap_stale = False
                logger_webrtc_input.info(
                    f"Wayland keymap owner ready ({len(self._wl_keymap_owner._map)} keysyms"
                    + (f", {len(previous._pressed)} held keys carried)" if previous is not None
                       else ")"))
            except Exception as e:
                self._wl_keymap_retry_at = time.monotonic() + 5.0
                logger_webrtc_input.warning(
                    f"Wayland keymap owner unavailable ({e}); retrying in 5s.")
                return None
            return self._wl_keymap_owner

    async def _wl_type_text(self, text: str) -> None:
        """Inject text through the app compositor's zwp_virtual_keyboard_manager_v1
        via pixelflux's one-shot in-process client: the first fallback rung under
        the seat keymap, above the clipboard paste. Raises when the compositor
        lacks the protocol or the injection fails, so callers can drop to the
        next rung the same way they would on any injection error. A failure
        against an auto-detected app compositor also drops the cached socket,
        so the next attempt re-detects a compositor that died or restarted
        under another name rather than aiming at a dead socket."""
        async with self._wl_typer_lock:
            if time.monotonic() < self._wl_typer_retry_at:
                raise RuntimeError(
                    "compositor does not advertise zwp_virtual_keyboard_manager_v1"
                    " (retry pending)")
            display = self._app_wayland_display()
            try:
                typer = getattr(self.wayland_input, 'type_keysyms_wayland', None)
                if typer is not None:
                    await asyncio.get_running_loop().run_in_executor(
                        None, typer, display, text_to_wayland_keysyms(text))
                else:
                    await asyncio.get_running_loop().run_in_executor(
                        None, self.wayland_input.type_text_wayland, display, text)
            except PixelfluxVkUnavailable:
                self._wl_typer_retry_at = time.monotonic() + 30.0
                raise
            except Exception:
                if self._app_wl_is_separate:
                    self._invalidate_app_wl_display()
                raise

    async def _inject_text_via_clipboard(self, text: str) -> bool:
        """Type `text` by replacing the clipboard with it, pasting via
        Shift+Insert, and restoring what was copied before. The route for
        compositors with no zwp_virtual_keyboard (KWin): the chord's two
        keysyms exist in every base layout, so this needs nothing beyond the
        data-control clipboard and ordinary key events. Held modifiers are
        lifted around the chord so they cannot corrupt the paste, and
        write_clipboard's baseline keeps the monitor from echoing the injected
        text back to clients. Returns True once the paste chord is sent."""
        if self._clipboard_inject_active:
            return False
        async with self._clipboard_inject_lock:
            self._clipboard_inject_active = True
            shift_keysym = 0xFFE1
            insert_keysym = 0xFF63
            held_modifiers = list(self.active_modifiers)
            try:
                for mod_keysym in held_modifiers:
                    await self.send_x11_keypress(mod_keysym, down=False)
                old_data, old_mime = await self.read_clipboard(use_binary=True)
                if not await self.write_clipboard(text):
                    return False
                # The sleeps are the focused app's margins to receive the new
                # offer and to finish fetching it before the selection is restored.
                await asyncio.sleep(0.02)
                await self.send_x11_keypress(shift_keysym, down=True)
                await self.send_x11_keypress(insert_keysym, down=True, neutralize=False)
                await self.send_x11_keypress(insert_keysym, down=False)
                await self.send_x11_keypress(shift_keysym, down=False)
                await asyncio.sleep(0.05)
                if old_data is not None:
                    await self.write_clipboard(old_data, old_mime or "text/plain")
                elif self.is_wayland:
                    await self._clear_injected_clipboard()
                return True
            except Exception as e:
                logger_webrtc_input.error(f"Clipboard text injection failed: {e}")
                return False
            finally:
                for mod_keysym in held_modifiers:
                    if mod_keysym in self.active_modifiers:
                        await self.send_x11_keypress(mod_keysym, down=True)
                self._clipboard_inject_active = False

    async def _clear_injected_clipboard(self) -> None:
        """Drop the selection the injection left behind when there was nothing
        to restore (an empty clipboard stays empty for the user)."""
        try:
            if self._has_separate_app_compositor():
                clear_fn = getattr(self.wayland_input, 'clipboard_clear_app', None)
                if clear_fn is not None:
                    await asyncio.get_running_loop().run_in_executor(
                        None, clear_fn, self._app_wayland_display())
            else:
                self.wayland_input.set_clipboard("text/plain", b"")
        except Exception as e:
            logger_webrtc_input.debug(f"post-injection clipboard clear failed: {e}")

    async def _type_keysym_fallback(self, keysym_number: int, down: bool = True) -> None:
        """Deliver a keysym the primary injector could not, resolving the newest
        mechanism first and degrading rung by rung. Wayland is subprocess-free:
        the keysym becomes text and goes through the in-process virtual-keyboard
        client, then the clipboard paste. X11 falls from the in-process XTEST
        shim to an xdotool key, then an xdotool type of the plain character."""
        if self.is_wayland:
            if not down:
                return
            char_to_type = keysym_to_character(keysym_number)

            if char_to_type:
                try:
                    await self._wl_type_text(char_to_type)
                except Exception as e:
                    if not await self._inject_text_via_clipboard(char_to_type):
                        logger_webrtc_input.warning(f"virtual-keyboard fallback failed: {e}")

            return

        if not self.xdisplay:
            return

        xdotool_key_arg = None
        char_for_type_cmd_fallback = None
        keysym_name_from_xlib = None

        if (keysym_number & 0xFF000000) == 0x01000000:
            unicode_codepoint = keysym_number & 0x00FFFFFF
            if 0 <= unicode_codepoint <= 0x10FFFF:
                xdotool_key_arg = f"U{unicode_codepoint:04X}"
                try:
                    char_for_type_cmd_fallback = chr(unicode_codepoint)
                except ValueError:
                    pass
            else:
                return
        else:
            keysym_name_from_xlib = XK.keysym_to_string(keysym_number)

            if keysym_name_from_xlib is None:
                # Decoded, not chr()'d: a keysym is a codepoint only in Latin-1,
                # and chr() typed Gujarati for the publishing block.
                char = keysym_to_character(keysym_number)
                if char is None:
                    return
                keysym_name_from_xlib = char
                char_for_type_cmd_fallback = char
            else:
                if len(keysym_name_from_xlib) == 1:
                    char_for_type_cmd_fallback = keysym_name_from_xlib
            
            xdotool_key_arg = keysym_name_from_xlib

            if len(keysym_name_from_xlib) == 1:
                char_code = ord(keysym_name_from_xlib)
                if char_code >= 0x80 or (char_code == keysym_number and char_code != 0x00):
                    xdotool_key_arg = f"U{char_code:04X}"
            # XK_sterling.
            elif keysym_number == 0x00a3:
                xdotool_key_arg = "sterling"
                if not char_for_type_cmd_fallback:
                    try: char_for_type_cmd_fallback = chr(0xA3)
                    except ValueError: pass

        if xdotool_key_arg is None:
            return

        action = "keydown" if down else "keyup"
        command_key = ["xdotool", action, xdotool_key_arg]

        try:
            process_key = await subprocess.create_subprocess_exec(
                *command_key,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE
            )
            stdout_key, stderr_key = await self._communicate_or_kill(process_key, 1.0, "xdotool keydown")
            if process_key.returncode != 0 or (stderr_key and (b"No such key name" in stderr_key or b"Error:" in stderr_key.lower())):
                char_to_type = char_for_type_cmd_fallback
                if not char_to_type and keysym_name_from_xlib and len(keysym_name_from_xlib) == 1:
                    char_to_type = keysym_name_from_xlib
                
                if down and char_to_type and (0x20 <= ord(char_to_type) <= 0x7E or ord(char_to_type) >= 0xA0) and char_to_type.isprintable():
                    command_type = ["xdotool", "type", "--clearmodifiers", "--", char_to_type]
                    try:
                        process_type = await subprocess.create_subprocess_exec(
                            *command_type,
                            stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE
                        )
                        await self._communicate_or_kill(process_type, 1.0, "xdotool type")
                    except (asyncio.TimeoutError, FileNotFoundError, Exception):
                        pass
        except (FileNotFoundError, asyncio.TimeoutError, Exception):
            pass

    async def send_x11_mouse(self, x: int, y: int, button_mask: int,
                             scroll_magnitude: int, relative: bool = False,
                             display_id: str = 'primary') -> None:
        """Apply one client pointer message on whichever backend this session uses.

        Moves the pointer (absolute coordinates are offset into the named
        display's region of the combined layout), then diffs button_mask
        against the held state bit by bit, emitting press/release, scroll
        clicks, or the Alt+Arrow back/forward chords.

        A delta is injected as a delta; the tracked position follows it only so
        that an absolute message has something to compare against, and it is
        bounded by the laid-out screen because the X server and the compositor
        bound the pointer the same way. The tracking stays an estimate even so
        — an application can move the pointer, and the framebuffer can be a few
        pixels wider than the layout — so the first absolute position after any
        delta warps unconditionally rather than trusting that comparison. Where
        no relative injection exists (a capture backend without it), the bound
        matters directly: the estimate is what gets injected.

        Args:
            x: Absolute X, or the X delta when relative.
            y: Absolute Y, or the Y delta when relative.
            button_mask: Client button bitmask (Pointer Events numbering). The
                eraser bit (button 5) folds into the primary button: neither
                the X core pointer nor wl_pointer has an eraser button, so the
                eraser clicks and drags like the pen tip, and OR-ing it keeps
                press/release balanced when the tip bit is set at the same time.
            scroll_magnitude: Wheel repeat count; 0 marks bits 3/4 as
                back/forward buttons instead of wheel ticks.
            relative: Interpret x/y as deltas.
            display_id: Display whose layout offset absolute coordinates use.
        """
        # Client-controlled; unbounded, the X11 scroll loop would block the event loop.
        try:
            scroll_magnitude = max(0, min(int(scroll_magnitude), 64))
        except (TypeError, ValueError):
            scroll_magnitude = 0
        if button_mask & MOUSE_MASK_BIT_ERASER:
            button_mask = (button_mask | MOUSE_MASK_BIT_PRIMARY) & ~MOUSE_MASK_BIT_ERASER
        was_stale = self.tracked_position_stale
        if relative:
            # XTEST carries the delta in an Int16; an overlarge value would fail
            # the request and take the button transitions on this message with it.
            x = max(-32768, min(32767, x))
            y = max(-32768, min(32767, y))
            final_x = self.last_x + x
            final_y = self.last_y + y
            edge_x, edge_y = 0, 0
            if self.data_server_instance and hasattr(self.data_server_instance, 'display_layouts'):
                edge_x, edge_y = layout_extent(self.data_server_instance.display_layouts)
            if edge_x > 0:
                final_x = max(0, min(final_x, edge_x - 1))
            if edge_y > 0:
                final_y = max(0, min(final_y, edge_y - 1))
            self.tracked_position_stale = True
        else:
            offset_x = 0
            offset_y = 0
            if self.data_server_instance and hasattr(self.data_server_instance, 'display_layouts'):
                # A socket with no registered display (shared viewer, input
                # handoff) renders the primary, whose offset is non-zero in
                # left/up arrangements.
                lookup_id = display_id or 'primary'
                layout = self.data_server_instance.display_layouts.get(lookup_id)
                if layout:
                    offset_x = layout.get('x', 0)
                    offset_y = layout.get('y', 0)
                elif lookup_id != 'primary':
                    # A secondary with no laid-out region must not inject at
                    # offset zero (its clicks would land on the primary); a held
                    # button self-heals on the next mask diff.
                    return
            final_x = x + offset_x
            final_y = y + offset_y
            self.tracked_position_stale = False

        position_changed = (was_stale or final_x != self.last_x or final_y != self.last_y)
        self.last_x = final_x
        self.last_y = final_y
        is_static_relative = relative and x == 0 and y == 0

        if self.wayland_input:
            if not is_static_relative:
                if relative:
                    if hasattr(self.wayland_input, 'inject_relative_mouse_move'):
                        self.wayland_input.inject_relative_mouse_move(float(x), float(y))
                    else:
                        self.wayland_input.inject_mouse_move(float(final_x), float(final_y))
                else:
                    self.wayland_input.inject_mouse_move(float(final_x), float(final_y))
            
            if button_mask != self.button_mask:
                for bit_index in range(8):
                    current_button_bit_value = (1 << bit_index)
                    button_state_changed = ((self.button_mask & current_button_bit_value) != \
                                            (button_mask & current_button_bit_value))

                    if button_state_changed:
                        is_pressed_now = (button_mask & current_button_bit_value) != 0
                        state = 1 if is_pressed_now else 0
                        mag = float(max(1, scroll_magnitude))

                        # evdev BTN_LEFT, BTN_MIDDLE, BTN_RIGHT.
                        if bit_index == 0:
                            self.wayland_input.inject_mouse_button(272, state)
                        elif bit_index == 1:
                            self.wayland_input.inject_mouse_button(274, state)
                        elif bit_index == 2:
                            self.wayland_input.inject_mouse_button(273, state)
                        
                        elif bit_index == 3:
                            if scroll_magnitude > 0:
                                if is_pressed_now:
                                    self.wayland_input.inject_mouse_scroll(0.0, 10.0 * mag)
                            else:
                                if is_pressed_now:
                                    # Queued behind pending keys like any key event:
                                    # direct injection could land between a kd and its ku.
                                    self._keyboard_enqueue_chord((
                                        (KEYSYM_ALT_L, True), (KEYSYM_LEFT_ARROW, True),
                                        (KEYSYM_LEFT_ARROW, False), (KEYSYM_ALT_L, False)))

                        elif bit_index == 4:
                            if scroll_magnitude > 0:
                                if is_pressed_now:
                                    self.wayland_input.inject_mouse_scroll(0.0, -10.0 * mag)
                            else:
                                if is_pressed_now:
                                    self._keyboard_enqueue_chord((
                                        (KEYSYM_ALT_L, True), (KEYSYM_RIGHT_ARROW, True),
                                        (KEYSYM_RIGHT_ARROW, False), (KEYSYM_ALT_L, False)))

                        elif bit_index == 6:
                            if scroll_magnitude > 0 and is_pressed_now:
                                self.wayland_input.inject_mouse_scroll(-10.0 * mag, 0.0)
                        elif bit_index == 7:
                            if scroll_magnitude > 0 and is_pressed_now:
                                self.wayland_input.inject_mouse_scroll(10.0 * mag, 0.0)

            self.button_mask = button_mask
            return
        if relative:
            if not is_static_relative:
                self.send_mouse(MOUSE_MOVE, (x, y))
        elif position_changed or button_mask != self.button_mask:
            # Button transitions warp unconditionally: an application may have
            # moved the pointer, and a press must land where the client aims.
            self.send_mouse(MOUSE_POSITION, (final_x, final_y))
        if button_mask != self.button_mask:
            for bit_index in range(8):
                current_button_bit_value = (1 << bit_index)
                button_state_changed = ((self.button_mask & current_button_bit_value) != \
                                        (button_mask & current_button_bit_value))

                if button_state_changed:
                    is_pressed_now = (button_mask & current_button_bit_value) != 0
                    
                    action_to_send = None
                    data_to_send = None
                    is_scroll_action = False
                    performed_keyboard_combo = False 

                    if bit_index == 0:
                        action_to_send = MOUSE_BUTTON
                        data_to_send = (MOUSE_BUTTON_PRESS if is_pressed_now else MOUSE_BUTTON_RELEASE, MOUSE_BUTTON_LEFT_ID)
                    elif bit_index == 1:
                        action_to_send = MOUSE_BUTTON
                        data_to_send = (MOUSE_BUTTON_PRESS if is_pressed_now else MOUSE_BUTTON_RELEASE, MOUSE_BUTTON_MIDDLE_ID)
                    elif bit_index == 2:
                        action_to_send = MOUSE_BUTTON
                        data_to_send = (MOUSE_BUTTON_PRESS if is_pressed_now else MOUSE_BUTTON_RELEASE, MOUSE_BUTTON_RIGHT_ID)
                    
                    elif bit_index == 3:
                        if scroll_magnitude > 0:
                            if is_pressed_now:
                                action_to_send = MOUSE_SCROLL_UP
                                is_scroll_action = True
                        else:
                            if is_pressed_now:
                                if self.keyboard:
                                    logger_webrtc_input.debug("Sending Alt+Left Arrow for Back")
                                    await self.send_x11_keypress(KEYSYM_ALT_L, down=True)
                                    await self.send_x11_keypress(KEYSYM_LEFT_ARROW, down=True)
                                    await self.send_x11_keypress(KEYSYM_LEFT_ARROW, down=False)
                                    await self.send_x11_keypress(KEYSYM_ALT_L, down=False)
                                    performed_keyboard_combo = True
                                else:
                                    logger_webrtc_input.warning("Keyboard not available for Alt+Left.")
                    elif bit_index == 4:
                        if scroll_magnitude > 0:
                            if is_pressed_now:
                                action_to_send = MOUSE_SCROLL_DOWN
                                is_scroll_action = True
                        else:
                            if is_pressed_now:
                                if self.keyboard:
                                    logger_webrtc_input.debug("Sending Alt+Right Arrow for Forward")
                                    await self.send_x11_keypress(KEYSYM_ALT_L, down=True)
                                    await self.send_x11_keypress(KEYSYM_RIGHT_ARROW, down=True)
                                    await self.send_x11_keypress(KEYSYM_RIGHT_ARROW, down=False)
                                    await self.send_x11_keypress(KEYSYM_ALT_L, down=False)
                                    performed_keyboard_combo = True
                                else:
                                    logger_webrtc_input.warning("Keyboard not available for Alt+Right.")
                    elif bit_index == 6:
                        if scroll_magnitude > 0 and is_pressed_now:
                            action_to_send = MOUSE_SCROLL_LEFT
                            is_scroll_action = True
                    elif bit_index == 7:
                        if scroll_magnitude > 0 and is_pressed_now:
                            action_to_send = MOUSE_SCROLL_RIGHT
                            is_scroll_action = True
                    if not performed_keyboard_combo and action_to_send is not None:
                        if is_scroll_action:
                            for _ in range(max(1, scroll_magnitude)):
                                self.send_mouse(action_to_send, None)
                        else:
                            self.send_mouse(action_to_send, data_to_send)
                
            self.button_mask = button_mask

        if not relative and self.xdisplay:
            # flush(), not sync(): a round trip per mouse event otherwise.
            self.xdisplay.flush()
    async def update_binary_clipboard_setting(self, enabled: bool) -> None:
        """Update the binary clipboard setting and restart the monitor if it is running."""
        async with self._binary_clipboard_lock:
            new_setting_str = "true" if enabled else "false"
            if self.enable_binary_clipboard == new_setting_str:
                return
            logger_webrtc_input.info(f"Binary clipboard setting changing to: {enabled}. Restarting monitor.")
            self.enable_binary_clipboard = new_setting_str
            if self.clipboard_monitor_task and not self.clipboard_monitor_task.done():
                self.stop_clipboard()
                self.clipboard_monitor_task.cancel()
                try:
                    await self.clipboard_monitor_task
                except asyncio.CancelledError:
                    pass
                self.clipboard_monitor_task = asyncio.create_task(self.start_clipboard())
    def _wayland_display_name(self) -> str:
        """The compositor's REAL socket name. The pixelflux compositor auto-picks
        the first free wayland-N socket, so the running backend is authoritative;
        the process env is next (stream_server mirrors the name there at bring-up)
        and --wayland-socket-index survives only as a legacy hint."""
        try:
            from pixelflux import get_wayland_display_name
            name = get_wayland_display_name()
            if name:
                return name
        except Exception:
            pass
        return (os.environ.get("WAYLAND_DISPLAY")
                or f"wayland-{self.wayland_socket_index}")

    def _app_wayland_display(self) -> str:
        """Socket of the compositor applications run under — the target for input
        injection and clipboard. It equals the capture compositor for a plain
        pixelflux session, but a nested session that pixelflux
        captures owns the apps on its own socket, so input and clipboard aimed at
        the capture compositor never reach them. Resolution order: the explicit
        app_wayland_display setting; else the single other wayland-* socket in
        XDG_RUNTIME_DIR besides the capture compositor's; else the capture socket.
        A distinct result is cached permanently; the capture fallback is negative-
        cached with a short TTL, so a nested compositor that appears after startup
        is still picked up within a couple seconds without relisting per call."""
        if self._app_wl_display_cached is not None:
            return self._app_wl_display_cached
        now = time.monotonic()
        if (self._app_wl_negcache is not None
                and (now - self._app_wl_negcache_at) < 2.0):
            return self._app_wl_negcache
        self._app_wl_negcache_at = now
        capture = self._wayland_display_name()
        override = (self.app_wayland_display or "").strip()
        if override:
            return self._adopt_app_wl_display(override, capture, "configured")
        resolved = None
        try:
            import stat as _stat
            runtime = os.environ.get("XDG_RUNTIME_DIR")
            cap_base = os.path.basename(capture)
            if runtime and os.path.isdir(runtime):
                others = sorted(
                    n for n in os.listdir(runtime)
                    if n.startswith("wayland-") and not n.endswith(".lock")
                    and n != cap_base
                    and _stat.S_ISSOCK(os.stat(os.path.join(runtime, n)).st_mode))
                # A stale wayland-* file left by a dead compositor must not be
                # adopted; it would silently break clipboard and input.
                others = [n for n in others
                          if self._wl_socket_live(os.path.join(runtime, n))]
                if len(others) == 1:
                    resolved = others[0]
                elif len(others) > 1:
                    # wayland-<N> is a compositor; a differently named socket is a
                    # relay a session listens on (see the module docstring).
                    numbered = [n for n in others
                                if n[len("wayland-"):].isdigit()]
                    if len(numbered) == 1:
                        resolved = numbered[0]
                    else:
                        logger_webrtc_input.warning(
                            "Multiple candidate app-compositor sockets %s; set "
                            "app_wayland_display to choose. Using capture compositor.",
                            others)
        except Exception as e:
            logger_webrtc_input.debug(f"App-compositor autodetect failed: {e}")
        if resolved and resolved != capture:
            return self._adopt_app_wl_display(resolved, capture, "auto-detected")
        self._app_wl_negcache = capture
        return capture

    @staticmethod
    def _wl_socket_live(path: str) -> bool:
        """True if a Wayland socket accepts a connection right now (rejecting a
        stale socket file with no listener)."""
        try:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.settimeout(0.2)
            try:
                s.connect(path)
                return True
            finally:
                s.close()
        except OSError:
            return False

    def _adopt_app_wl_display(self, resolved: str, capture: str, how: str) -> str:
        """Cache the resolved app compositor and, when it is distinct from the
        capture compositor, hand it to pixelflux over the Python ABI so its
        Computer-Use backend targets the same session. pixelflux keeps its own
        PIXELFLUX_APP_WAYLAND_DISPLAY env fallback for standalone use without
        selkies."""
        self._app_wl_display_cached = resolved
        self._app_wl_negcache = None
        self._app_wl_is_separate = resolved != capture
        if self._app_wl_is_separate:
            logger_webrtc_input.info(
                f"Wayland app compositor '{resolved}' ({how}); routing input + "
                f"clipboard there, capture stays on '{capture}'.")
            try:
                self.wayland_input.set_app_wayland_display(resolved)
            except Exception as e:
                logger_webrtc_input.debug(
                    f"pixelflux set_app_wayland_display failed: {e}")
            self._schedule_session_scale()
            self._schedule_spare_screen_hold()
            self._schedule_seat_layout_restore()
        return resolved

    def app_session(self) -> dict:
        """Where the session's applications run: ``x11_display`` (the X server
        they connect to, if any), ``wayland_display`` (the compositor socket,
        when they are Wayland clients) and ``type`` ("x11" or "wayland").

        X11 backend: the server's own DISPLAY. Wayland backend: apps under a
        nested session compositor use its socket and the Xwayland it spawned (the
        one live X server; $DISPLAY when that is live); with no nested compositor
        a rootful Xwayland on $DISPLAY hosts an X11 desktop — its apps are X
        clients and must not be offered the capture compositor's socket, which
        would take them out of the desktop as fullscreen toplevels. The server
        on $DISPLAY counts only when it is that Xwayland (not a leftover
        Xvfb/Xorg holding the number); otherwise the apps are Wayland clients of
        the capture compositor itself.
        """
        env_display = os.environ.get("DISPLAY", "").strip() or None
        if not self.is_wayland:
            return {"x11_display": env_display, "wayland_display": None, "type": "x11"}
        wayland = self._app_wayland_display()
        if self._has_separate_app_compositor():
            live = live_x_displays()
            x11 = env_display if env_display in live else (live[0] if live else None)
            return {"x11_display": x11, "wayland_display": wayland, "type": "wayland"}
        if env_display and x_display_live(env_display) and x_display_is_xwayland(env_display):
            return {"x11_display": env_display, "wayland_display": None, "type": "x11"}
        return {"x11_display": None, "wayland_display": wayland, "type": "wayland"}

    def app_launch_env(self) -> dict:
        """Environment for a client-requested command: the server's, with
        DISPLAY / WAYLAND_DISPLAY / XDG_SESSION_TYPE set for the session the
        applications run in (app_session) and the session bus plus desktop
        identity adopted from that session's processes when the server has
        none of its own. The adopted subset is cached per session and refreshed
        when its bus stops answering; an empty scan is negative-cached briefly
        so a command burst on a session with no bus does not rescan /proc (a
        synchronous walk on the loop) each time."""
        session = self.app_session()
        env = dict(os.environ)
        for key in ("DISPLAY", "WAYLAND_DISPLAY"):
            env.pop(key, None)
        if session["x11_display"]:
            env["DISPLAY"] = session["x11_display"]
        if session["wayland_display"]:
            env["WAYLAND_DISPLAY"] = session["wayland_display"]
        env["XDG_SESSION_TYPE"] = session["type"]
        key = (session["x11_display"], session["wayland_display"])
        adopted = self._session_env_cache.get(key)
        if adopted is None or not dbus_address_live(
                adopted.get("DBUS_SESSION_BUS_ADDRESS", "")):
            now = time.monotonic()
            if adopted is None and (now - self._session_env_empty_at.get(key, -1e9)
                                    ) < self._session_env_negcache_ttl:
                adopted = {}
            else:
                adopted = session_environment(
                    session["x11_display"], session["wayland_display"])
                if adopted:
                    self._session_env_cache[key] = adopted
                    self._session_env_empty_at.pop(key, None)
                    logger_webrtc_input.info(
                        f"Application launches adopt the session on {key}: "
                        f"{', '.join(sorted(adopted))}")
                else:
                    self._session_env_cache.pop(key, None)
                    self._session_env_empty_at[key] = now
        for name, value in adopted.items():
            env.setdefault(name, value)
        return env

    def app_terminal(self) -> Optional[str]:
        """Terminal command prefix clients launch proot-apps under (the
        terminal plus its run-this flag, e.g. `xterm -e`): the first installed
        one for the windowing system the session's applications use (foot on a
        Wayland session, st on an X11 one), published as app_terminal."""
        session = self.app_session()
        return first_installed(WAYLAND_APP_TERMINALS if session["type"] == "wayland"
                               else X11_APP_TERMINALS)

    def _invalidate_app_wl_display(self) -> None:
        """Drop the cached app-compositor resolution so the next call re-detects
        it — used when a connection to it fails (a nested compositor that died or
        restarted on a different socket name)."""
        if self._app_wl_display_cached:
            try:
                self.wayland_input.clipboard_unwatch_app(self._app_wl_display_cached)
            except Exception:
                pass
        self._app_wl_display_cached = None
        self._app_wl_is_separate = False
        self._app_wl_negcache = None
        self._app_wl_negcache_at = 0.0

    def _invalidate_app_wl_display_if_dead(self, display: str) -> None:
        """Re-detect the app compositor when its socket no longer answers (a
        nested compositor that died or restarted under another name); a live
        socket that merely failed one request is kept."""
        if not display:
            return
        runtime = os.environ.get("XDG_RUNTIME_DIR") or ""
        path = display if display.startswith("/") else os.path.join(runtime, display)
        if not self._wl_socket_live(path):
            logger_webrtc_input.info(
                f"Wayland app compositor '{display}' no longer answers; re-detecting.")
            self._invalidate_app_wl_display()

    def _has_separate_app_compositor(self) -> bool:
        """True when apps live under a compositor distinct from pixelflux's own,
        so pixelflux's keymap overlay and its selection never reach them. Resolves
        (throttled) then reads the cached flag, so it costs no per-call FFI once
        settled."""
        self._app_wayland_display()
        return self._app_wl_is_separate

    def _size_session_screen(self, display: str, display_index: int, scale: float,
                             size: Optional[Tuple[int, int]]) -> bool:
        """Give the session compositor's screen its scale, and its mode too when
        the caller knows the size the screen is about to carry. Blocking.

        A session lays its desktop out once per applied configuration, so a scale
        that arrives on its own leaves the screen at the old mode under the new
        scale — a fraction of the size it ends at, which is what a client that
        does not lay out again keeps. Older pixelflux builds have no combined
        call and take the scale alone.
        """
        geometry = getattr(self.wayland_input, "set_app_screen_geometry", None)
        if geometry is not None and size and size[0] > 0 and size[1] > 0:
            return bool(geometry(display, display_index,
                                 int(size[0]), int(size[1]), scale))
        return bool(self.wayland_input.set_app_output_scale(
            display, display_index, scale))

    async def realize_wayland_dpi(self, dpi: Any, display_index: int = 0,
                                  size: Optional[Tuple[int, int]] = None) -> float:
        """Apply a DPI on the Wayland backend and return the capture output
        scale it leaves behind.

        Applications draw larger when the compositor they are on scales its own
        output, so a nested session is scaled through its output management and
        the capture keeps 1.0: scaling the capture instead would halve the
        logical size the session is handed and upscale the whole desktop. A
        session that manages no outputs for clients (KWin) takes the capture
        output's scale, which it follows, and so does a plain pixelflux session,
        where the capture output is the only screen there is. XWayland
        applications need nothing merged: they run in the compositor's logical
        space and are scaled with it.

        Args:
            dpi: The desktop DPI to realize; 96 is unity.
            display_index: Which of the session's screens backs this display.
            size: The pixel size that screen is about to carry, when the caller
                already knows it, so the mode and the scale land together.

        Returns:
            The scale left for the capture output: 1.0 once a session absorbed
            it, the full scale otherwise.
        """
        try:
            scale = max(0.1, float(dpi) / 96.0)
        except (TypeError, ValueError):
            return 1.0
        try:
            if not self._has_separate_app_compositor():
                return scale
            display = self._app_wayland_display()
            applied = await asyncio.to_thread(
                self._size_session_screen, display, display_index, scale, size)
        except Exception as e:
            logger_webrtc_input.debug(f"Session output scale failed: {e}")
            return scale
        if applied:
            logger_webrtc_input.info(
                f"Session compositor screen {display_index} scaled to {scale}.")
            return 1.0
        return scale

    # Size of a held spare session screen: small enough to leave the desktop's
    # centre on the screen that is shown, large enough for a compositor to lay out.
    SPARE_SCREEN_SIZE = (320, 240)

    def resync_session_screens(self) -> None:
        """Re-hold the session's spare screens after the output set changed.

        The nested session opens the screens it was started with, and which of
        them a capture drives changes as displays come and go, so the hold is
        recomputed whenever a layout pass creates or destroys an output.
        """
        if self.wayland_input is None:
            return
        self._schedule_spare_screen_hold()

    def _schedule_spare_screen_hold(self) -> None:
        """A nested session opens the screens it was started with, whether or
        not the capture drives that many: the extra ones stretch its desktop
        onto a screen nobody sees, which is where a client that centres itself
        then lands. Hold them small until a display arrives for them —
        pixelflux resizes one to its full size the moment it gets an output,
        and back when it loses one."""
        try:
            asyncio.get_running_loop()
        except RuntimeError:
            return
        self._spawn_task(self._hold_spare_screens())

    async def _hold_spare_screens(self) -> None:
        """Hold every session screen without a capture output at SPARE_SCREEN_SIZE."""
        display = self._app_wayland_display()
        try:
            keep = max(1, len(await asyncio.to_thread(self.wayland_input.list_outputs)))
            held = await asyncio.to_thread(
                self.wayland_input.hold_spare_app_screens, display, keep,
                *self.SPARE_SCREEN_SIZE)
        except Exception as e:
            logger_webrtc_input.debug(f"Holding spare session screens failed: {e}")
            return
        if held:
            logger_webrtc_input.info(
                f"Session compositor has {held} screen(s) with no capture output; "
                f"held at {self.SPARE_SCREEN_SIZE[0]}x{self.SPARE_SCREEN_SIZE[1]}.")

    def _schedule_session_scale(self) -> None:
        """A session compositor was just adopted: hand it the effective DPI as
        its output scale. A scale applied before it existed landed on the
        capture output, which the session does not follow. An operator-set DPI
        governs the desktop (client syncs never reach it then); otherwise the
        last client-synced DPI does. 96 is unity, so nothing to apply."""
        try:
            if settings._overridden.get("scaling_dpi", False):
                dpi = int(float(settings.scaling_dpi))
            else:
                dpi = int(float(getattr(self, "system_dpi", 96) or 96))
        except (TypeError, ValueError, AttributeError):
            return
        if dpi == 96:
            return
        try:
            asyncio.get_running_loop()
        except RuntimeError:
            return
        self._spawn_task(self.realize_wayland_dpi(dpi))

    async def _get_file(self, file_path: str, target_mime: str) -> tuple:
        """Read a clipboard-referenced file, bounded to 10MB; (bytes, mime) or (None, None)."""
        max_clipboard_file_size = 10 * 1024 * 1024
        try:
            file_size = await asyncio.to_thread(os.path.getsize, file_path)
            if file_size > max_clipboard_file_size:
                logger_webrtc_input.warning(
                    "Skipping clipboard file %s: %d bytes exceeds 10MB limit", 
                    file_path, file_size
                )
                return None, None
            async with aiofiles.open(file_path, 'rb') as f:
                file_data = await f.read(max_clipboard_file_size + 1)
                if len(file_data) > max_clipboard_file_size:
                    logger_webrtc_input.warning(
                        "Skipping clipboard file %s: file grew beyond 10MB limit during read (%d bytes)",
                        file_path, len(file_data)
                    )
                    return None, None
                return file_data, target_mime
        except OSError as e:
            logger_webrtc_input.warning("Failed to access clipboard file %s: %s", file_path, e)
            return None, None

    async def _kill_and_reap_process(self, proc: Any, description: str) -> None:
        """Kill a timed-out helper process and wait briefly so it is reaped."""
        logger_webrtc_input.warning(
            "Timed out waiting for clipboard command '%s' pid=%s; killing it.",
            description,
            getattr(proc, "pid", "unknown"),
        )
        try:
            proc.kill()
        except ProcessLookupError:
            pass
        try:
            await asyncio.wait_for(proc.wait(), timeout=1.0)
        except (asyncio.TimeoutError, ProcessLookupError):
            logger_webrtc_input.warning(
                "Timed-out clipboard command '%s' pid=%s could not be reaped promptly.",
                description,
                getattr(proc, "pid", "unknown"),
            )

    async def _communicate_or_kill(self, proc: Any, timeout: float,
                                   description: str,
                                   input: Optional[bytes] = None) -> tuple:
        """proc.communicate with a deadline; on timeout the process is killed and
        reaped before TimeoutError is re-raised."""
        try:
            return await asyncio.wait_for(proc.communicate(input=input), timeout=timeout)
        except asyncio.TimeoutError:
            await self._kill_and_reap_process(proc, description)
            raise

    def _clipboard_has_consumers(self) -> bool:
        """Whether any connected client would receive an outbound clipboard send."""
        if getattr(self.rtc_app, "mode", None) != "websockets":
            return True
        server = self.data_server_instance or getattr(self.rtc_app, "data_streaming_server", None)
        return bool(server and getattr(server, "clients", None))

    async def _app_clipboard_read(self, use_binary: bool) -> tuple:
        """Read the selection of the compositor the apps use over the pixelflux
        data-control ABI; (None, None) when it is empty or unreadable."""
        read_fn = getattr(self.wayland_input, 'clipboard_read_app', None)
        types_fn = getattr(self.wayland_input, 'clipboard_types_app', None)
        if read_fn is None or types_fn is None:
            # A pixelflux without the data-control ABI reports no data, not failure.
            return None, None
        display = self._app_wayland_display()
        loop = asyncio.get_running_loop()
        try:
            available_types = await loop.run_in_executor(
                None, types_fn, display)
            self._app_clip_read_failure = None
            if use_binary:
                image_mimes = ['image/png', 'image/jpeg', 'image/bmp', 'image/webp',
                               'image/svg+xml', 'image/svg']
                target_mime = next((m for m in image_mimes if m in available_types), None)
                if target_mime:
                    data = await loop.run_in_executor(
                        None, read_fn, display, target_mime)
                    if data:
                        return bytes(data), target_mime
            text_mimes = ['text/plain;charset=utf-8', 'text/plain',
                          'UTF8_STRING', 'STRING', 'TEXT']
            source_mime = next((m for m in text_mimes if m in available_types), None)
            if source_mime:
                data = await loop.run_in_executor(None, read_fn, display, source_mime)
                if data is not None:
                    return data.decode('utf-8', errors='replace'), 'text/plain'
        except Exception as e:
            failure = (display, str(e))
            log = (logger_webrtc_input.debug if failure == self._app_clip_read_failure
                   else logger_webrtc_input.warning)
            self._app_clip_read_failure = failure
            log(f"data-control clipboard read failed: {e}")
            self._invalidate_app_wl_display_if_dead(display)
        return None, None

    async def read_clipboard(self, use_binary: bool = False) -> tuple:
        """Read the session clipboard.

        Wayland is fully native. A rootful Xwayland on the capture compositor
        bridges no selection of its own, so the X11 desktop's copies are read
        from it first (a selection still owned there is our own write, for
        which the compositor side is authoritative); then the compositor
        callback's cache, which holds the capture compositor's selection and
        is the wrong session under a separate app compositor, where the
        data-control client reads whichever socket the apps use instead. X11
        uses the XFixes monitor with an xclip fallback.

        Args:
            use_binary: Prefer image targets (and file-manager uri-lists)
                before falling back to text.

        Returns:
            (data, mime): text as str with mime 'text/plain', images as bytes
            with their mime, or (None, None) when nothing is readable.
        """
        if self.is_wayland:
            monitor = await self._ensure_x11_clipboard_monitor_async()
            if monitor is not None and not monitor.owns_selection():
                try:
                    loop = asyncio.get_running_loop()
                    data, mime = await loop.run_in_executor(None, monitor.read, use_binary)
                    if data is not None:
                        return data, mime
                except Exception as e:
                    logger_webrtc_input.warning(f"X11 clipboard read on the Wayland session failed: {e}")
            cached = (None if self._has_separate_app_compositor()
                      else getattr(self, '_wl_native_last', None))
            if cached is not None:
                raw, native_mime = cached
                if native_mime.startswith('image/'):
                    if use_binary:
                        return bytes(raw), native_mime
                else:
                    return bytes(raw).decode('utf-8', errors='replace'), 'text/plain'
            return await self._app_clipboard_read(use_binary)
        monitor = await self._ensure_x11_clipboard_monitor_async()
        if monitor is not None:
            try:
                loop = asyncio.get_running_loop()
                return await loop.run_in_executor(None, monitor.read, use_binary)
            except Exception as e:
                logger_webrtc_input.warning(f"native X11 clipboard read failed, using xclip: {e}")
        try:
            proc_targets = await subprocess.create_subprocess_exec(
                "xclip", "-selection", "clipboard", "-o", "-t", "TARGETS",
                stdout=subprocess.PIPE, stderr=subprocess.PIPE
            )
            stdout_targets, _ = await self._communicate_or_kill(proc_targets, 1, "xclip TARGETS")
            if proc_targets.returncode != 0:
                return None, None
            targets = stdout_targets.decode().strip().split('\n')
            if use_binary:
                for mime_type in ['image/png', 'image/jpeg', 'image/bmp', 'image/webp',
                                  'image/svg+xml', 'image/svg']:
                    if mime_type in targets:
                        proc_data = await subprocess.create_subprocess_exec(
                            "xclip", "-selection", "clipboard", "-o", "-t", mime_type,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE
                        )
                        stdout_data, _ = await self._communicate_or_kill(proc_data, 3, f"xclip {mime_type}")
                        if proc_data.returncode == 0 and stdout_data:
                            return stdout_data, mime_type

                # File-manager copy: a text/uri-list of file:// URIs.
                if 'text/uri-list' in targets:
                    proc_data = await subprocess.create_subprocess_exec(
                        "xclip", "-selection", "clipboard", "-o", "-t", "text/uri-list",
                        stdout=subprocess.PIPE, stderr=subprocess.PIPE
                    )
                    stdout_data, _ = await self._communicate_or_kill(proc_data, 1, "xclip text/uri-list")
                    if proc_data.returncode == 0 and stdout_data:
                        lines = stdout_data.decode("utf-8", errors="replace").splitlines()
                        for line in lines:
                            line = line.strip()
                            if not line or line.startswith("#"):
                                continue
                            parsed_uri = urllib.parse.urlparse(line)
                            if parsed_uri.scheme == 'file':
                                file_path = urllib.request.url2pathname(parsed_uri.path)
                                if os.path.isfile(file_path):
                                    ext = os.path.splitext(file_path)[1].lower()
                                    mime_map = {
                                        '.png': 'image/png', '.jpg': 'image/jpeg',
                                        '.jpeg': 'image/jpeg', '.bmp': 'image/bmp',
                                        '.webp': 'image/webp', '.svg': 'image/svg+xml'
                                    }
                                    if ext in mime_map:
                                        target_mime = mime_map[ext]
                                        return await self._get_file(file_path, target_mime)
                                
            if 'UTF8_STRING' in targets:
                proc_text = await subprocess.create_subprocess_exec(
                    "xclip", "-selection", "clipboard", "-o", "-t", "UTF8_STRING",
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE
                )
                stdout_text, _ = await self._communicate_or_kill(proc_text, 1, "xclip UTF8_STRING")
                if proc_text.returncode == 0:
                    return stdout_text.decode(), 'text/plain'
            return None, None
        except FileNotFoundError:
            if not self._xclip_missing_warned:
                self._xclip_missing_warned = True
                logger_webrtc_input.warning(
                    "xclip is not installed; the clipboard polling rung has "
                    "nothing to read with.")
            return None, None
        except Exception as e:
            logger_webrtc_input.warning(f"Error reading clipboard with xclip: {e}", exc_info=True)
            return None, None

    async def write_clipboard(self, data: Union[str, bytes],
                              mime_type: str = "text/plain") -> bool:
        """Set the session clipboard, native first with forked fallbacks.

        Wayland sets pixelflux's own selection in-process (or writes through
        the data-control client to a separate app compositor); X11 offers
        through the XFixes monitor's connection, falling back to an xclip
        fork. The written bytes become the monitor baseline BEFORE the write
        so the ownership-change event cannot echo a client's own content back
        (that echo loop saturates the transport). On the capture compositor
        the payload is also offered on the unbridged X server (a rootful
        Xwayland sees no Wayland selection) so the X11 desktop can paste it.

        Returns:
            True when the clipboard was set (an empty payload is a no-op True).
        """
        if not data:
            return True
        input_bytes = data if isinstance(data, bytes) else data.encode('utf-8')
        self._clipboard_last_bytes = input_bytes

        if self.is_wayland:
            if not self._has_separate_app_compositor():
                try:
                    self.wayland_input.set_clipboard(mime_type, input_bytes)
                    # The compositor does not echo its own selection back; a
                    # later read (another client joining) must still see it.
                    self._wl_native_last = (input_bytes, mime_type)
                    ok = True
                except Exception as e:
                    logger_webrtc_input.warning(f"native wayland clipboard set failed: {e}")
                    ok = False
                monitor = await self._ensure_x11_clipboard_monitor_async()
                if monitor is not None:
                    try:
                        loop = asyncio.get_running_loop()
                        if await loop.run_in_executor(None, monitor.offer, input_bytes, mime_type):
                            ok = True
                    except Exception as e:
                        logger_webrtc_input.warning(f"X11 clipboard offer on the Wayland session failed: {e}")
                return ok
            # Text is offered under every conventional target; apps pick their own.
            if mime_type == "text/plain":
                entries = [(m, input_bytes) for m in (
                    "text/plain;charset=utf-8", "text/plain",
                    "UTF8_STRING", "STRING", "TEXT")]
            else:
                entries = [(mime_type, input_bytes)]
            try:
                await asyncio.get_running_loop().run_in_executor(
                    None, self.wayland_input.clipboard_write_app,
                    self._app_wayland_display(), entries)
                return True
            except Exception as e:
                logger_webrtc_input.warning(f"data-control clipboard write failed: {e}")
                self._invalidate_app_wl_display_if_dead(self._app_wayland_display())
                return False

        env = os.environ.copy()
        if 'LANG' not in env or env['LANG'] == 'C':
            env['LANG'] = 'C.UTF-8'
        monitor = await self._ensure_x11_clipboard_monitor_async()
        if monitor is not None:
            try:
                loop = asyncio.get_running_loop()
                ok = await loop.run_in_executor(None, monitor.offer, input_bytes, mime_type)
                if ok:
                    return True
            except Exception as e:
                logger_webrtc_input.warning(f"native X11 clipboard offer failed, using xclip: {e}")
        try:
            is_text = mime_type == "text/plain"
            target_mime = "UTF8_STRING" if is_text else mime_type
            process = await subprocess.create_subprocess_exec(
                "xclip", "-selection", "clipboard", "-i", "-t", target_mime,
                stdin=subprocess.PIPE,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                env=env
            )
            # One deadline for the stdin write plus exit: a payload larger than
            # the pipe buffer cannot wedge on a stalled xclip.
            await self._communicate_or_kill(
                process, 2.0, f"xclip -i {target_mime}", input_bytes)
            return_code = process.returncode
            if return_code == 0:
                return True
            else:
                logger_webrtc_input.warning(f"xclip process exited with non-zero code: {return_code}")
                return False
        except asyncio.TimeoutError:
            logger_webrtc_input.warning("Timeout waiting for xclip process to terminate.")
            return False
        except FileNotFoundError:
            if not self._xclip_missing_warned:
                self._xclip_missing_warned = True
                logger_webrtc_input.warning(
                    "xclip is not installed; the clipboard polling rung has "
                    "nothing to write with.")
            return False
        except Exception:
            logger_webrtc_input.warning("Error writing to clipboard with xclip", exc_info=True)
            return False
    def _x11_session_display(self) -> Optional[str]:
        """Display of an X server whose selections nothing bridges into the
        Wayland session: on the Wayland backend without a nested session
        compositor, a live server on $DISPLAY is a rootful (or unmanaged)
        Xwayland client of the capture compositor hosting an X11 desktop, and
        Xwayland bridges no selection by itself. A nested compositor's own XWM
        bridges its Xwayland into the selection selkies already watches, and on
        the X11 backend the display is the session itself, so both answer None.
        The server must be an Xwayland (not a leftover Xvfb/Xorg that merely
        holds the display number), or nothing on it belongs to this session."""
        if not self.is_wayland or self._has_separate_app_compositor():
            return None
        name = os.environ.get("DISPLAY", "").strip()
        if not name or not x_display_live(name) or not x_display_is_xwayland(name):
            return None
        return name

    def _ensure_x11_clipboard_monitor(self, display_name: Optional[str] = None) -> Optional[_X11ClipboardMonitor]:
        """Get-or-create the event-driven X11 monitor; None if unavailable. On
        the Wayland backend ``display_name`` names the unbridged X server to
        watch (see _x11_session_display) and None means there is none."""
        if self._x11_clipboard_monitor is not None:
            return self._x11_clipboard_monitor
        if not X11_LIBS_AVAILABLE or (self.is_wayland and display_name is None):
            return None
        try:
            self._x11_clipboard_monitor = _X11ClipboardMonitor(display_name)
            self._x11_monitor_unavail_logged = False
            where = f" on {display_name} (unbridged X server of the Wayland session)" if self.is_wayland else ""
            logger_webrtc_input.info(f"X11 clipboard: XFixes event monitor active{where}.")
        except Exception as e:
            log = (logger_webrtc_input.debug if self._x11_monitor_unavail_logged
                   else logger_webrtc_input.info)
            # The Wayland backend has no X poll rung (the compositor feed carries
            # the selection), so the monitor is simply retried.
            fallback = "retrying" if self.is_wayland else "falling back to polling"
            log(f"X11 clipboard: XFixes monitor unavailable ({e}); {fallback}.")
            self._x11_monitor_unavail_logged = True
            self._x11_clipboard_monitor = None
        return self._x11_clipboard_monitor

    async def _ensure_x11_clipboard_monitor_async(self) -> Optional[_X11ClipboardMonitor]:
        """Off-loop get-or-create: construction opens its own X connection, and a
        server disrupted mid-session (the respawn case) would stall the event loop
        for the whole bounded handshake. The lock keeps concurrent callers from
        racing two monitors into existence. A failed build backs off before the
        next attempt, so pollers do not hammer a dead display with connection
        attempts — and a display that comes up later is still re-probed."""
        if self._x11_clipboard_monitor is not None:
            return self._x11_clipboard_monitor
        if time.monotonic() < self._x11_monitor_retry_at:
            return None
        async with self._x11_monitor_build_lock:
            if self._x11_clipboard_monitor is not None:
                return self._x11_clipboard_monitor
            display_name = self._x11_session_display() if self.is_wayland else None
            if self.is_wayland and display_name is None:
                # No unbridged X server right now; no backoff, since the socket
                # probe is cheap and an Xwayland starting later is caught within a tick.
                return None
            monitor = await asyncio.to_thread(self._ensure_x11_clipboard_monitor, display_name)
            if monitor is None:
                self._x11_monitor_retry_at = time.monotonic() + 10.0
            return monitor

    @staticmethod
    async def _wait_x11_or_compositor_change(x11_monitor: _X11ClipboardMonitor,
                                             queue: "asyncio.Queue", timeout: float) -> tuple:
        """Wait for whichever signals first: the X server's selection-owner
        change or a compositor clipboard delivery. Returns (changed, item) where
        item is the compositor delivery, if that is what fired."""
        x_wait = asyncio.ensure_future(x11_monitor.wait_change(timeout))
        q_get = asyncio.ensure_future(queue.get())
        try:
            done, _ = await asyncio.wait({x_wait, q_get}, timeout=timeout + 0.5,
                                         return_when=asyncio.FIRST_COMPLETED)
            item = q_get.result() if q_get in done else None
            x_changed = bool(x_wait.result()) if x_wait in done else False
            return (item is not None or x_changed), item
        finally:
            # Whichever did not fire (both on cancellation), so no queue.get() lingers.
            for fut in (x_wait, q_get):
                if not fut.done():
                    fut.cancel()

    def _arm_wayland_native_clipboard(self) -> Optional[asyncio.Queue]:
        """Register the compositor clipboard callback (fork-free watch+read);
        returns the delivery queue, or None when the API is unavailable.

        Registered once per monitor start: the compositor keeps the callback
        across capture stops and starts, and every registration stages a
        fresh read of the whole current selection (so a copy made before the
        monitor ran is delivered), which must not recur while idle."""
        if not (self.wayland_input is not None
                and hasattr(self.wayland_input, 'set_clipboard_callback')):
            return None
        try:
            loop = asyncio.get_running_loop()
            queue = asyncio.Queue(maxsize=4)

            def _on_clip(mime, data):
                # Cache for on-demand reads (cr/REQUEST_CLIPBOARD).
                self._wl_native_last = (bytes(data), mime)

                def _put():
                    if queue.full():
                        queue.get_nowait()
                    queue.put_nowait((data, mime))
                loop.call_soon_threadsafe(_put)

            self.wayland_input.set_clipboard_callback(_on_clip)
            self._wl_native_arm_failure = None
            logger_webrtc_input.info("Wayland clipboard: native compositor callback active (no polling).")
            return queue
        except Exception as e:
            if str(e) != getattr(self, '_wl_native_arm_failure', None):
                self._wl_native_arm_failure = str(e)
                logger_webrtc_input.warning(
                    f"Wayland clipboard: native callback failed to arm ({e}); retrying.")
            return None

    async def _arm_app_compositor_watch(self) -> Optional[asyncio.Queue]:
        """Selection-change signals from the app compositor over the pixelflux
        data-control ABI (fork-free); returns the signal queue, or None when no
        watch could be armed — the monitor loop then polls that compositor and
        retries the arm each tick. The watch itself only reports a failed
        data-control handshake on its own thread, so the same handshake is
        made here first (one-shot, off the loop): a compositor that offers
        neither ext- nor zwlr-data-control is known at arm time instead of
        leaving a watch that never fires. Each failure is logged once."""
        watch_fn = getattr(self.wayland_input, 'clipboard_watch_app', None)
        if watch_fn is None:
            # Without the data-control ABI the native callback monitor covers this display.
            return None
        display = self._app_wayland_display()
        try:
            probe = getattr(self.wayland_input, 'clipboard_types_app', None)
            if probe is not None:
                await asyncio.to_thread(probe, display)
            loop = asyncio.get_running_loop()
            queue = asyncio.Queue(maxsize=4)

            def _on_change(mimes):
                def _put():
                    if queue.full():
                        queue.get_nowait()
                    queue.put_nowait(mimes)
                loop.call_soon_threadsafe(_put)

            watch_fn(display, _on_change)
            self._app_watch_failure = None
            logger_webrtc_input.info(
                "Wayland clipboard: app-compositor data-control watch active (no forks).")
            return queue
        except Exception as e:
            failure = (display, str(e))
            if failure != self._app_watch_failure:
                self._app_watch_failure = failure
                logger_webrtc_input.warning(
                    f"Wayland clipboard: no selection watch on app compositor "
                    f"'{display}' ({e}); polling it instead.")
            self._invalidate_app_wl_display_if_dead(display)
            return None

    async def start_clipboard(self) -> None:
        """Run the outbound clipboard monitor until stop_clipboard.

        Event-driven on every rung that offers events (XFixes monitor,
        compositor callback, app-compositor data-control watch), with xclip
        polling as the last X11 fallback; each pass reads the selection,
        compares against the echo baseline, and broadcasts real changes to
        clients. The first consumer after a consumer-less stretch gets the
        current selection once, since change events during that stretch were
        skipped and a copy made before any client connected would otherwise
        never arrive.

        The compositor callback watches the capture compositor's selection;
        with a separate app compositor the apps' copies land on its selection
        instead, watched through the data-control client, which is (re)armed
        inside the loop so a nested session appearing after startup — or
        restarting on a new socket — is picked up. A mode switch stops the
        running monitor and starts the replacement immediately, so the
        singleton guard waits briefly for the stopped loop to unwind instead
        of refusing, which would leave the session with no outbound clipboard
        until the setting is toggled.
        """
        if self.enable_clipboard not in ["true", "out"]:
            logger_webrtc_input.info("Skipping outbound clipboard service."); return

        for _ in range(50):
            if not self._clipboard_monitor_active:
                break
            await asyncio.sleep(0.1)
        if self._clipboard_monitor_active:
            logger_webrtc_input.info("Clipboard monitor already running; not starting a second instance.")
            return
        self._clipboard_monitor_active = True

        logger_webrtc_input.info(f"Clipboard monitor running (binary mode: {self.enable_binary_clipboard in ['true', 'out']})")
        self.clipboard_running = True
        x11_monitor = await self._ensure_x11_clipboard_monitor_async()
        wl_native_queue = (self._arm_wayland_native_clipboard()
                           if self.is_wayland and not self._has_separate_app_compositor()
                           else None)
        wl_native_item = None
        app_watch_queue = None
        app_watch_display = None
        # Primed so the first pass publishes the current content once.
        first_pass = True
        had_consumers = False
        try:
            while self.clipboard_running:
                try:
                    wl_native_item = None
                    if self.is_wayland and self._has_separate_app_compositor():
                        if x11_monitor is not None:
                            # A nested session compositor appeared: its XWM bridges
                            # its own Xwayland, so the app-compositor watch takes over.
                            try:
                                x11_monitor.close()
                            except Exception:
                                pass
                            self._x11_clipboard_monitor = None
                            x11_monitor = None
                        disp = self._app_wayland_display()
                        if app_watch_queue is None or disp != app_watch_display:
                            q = await self._arm_app_compositor_watch()
                            if q is not None:
                                app_watch_queue, app_watch_display = q, disp
                    elif self.is_wayland and wl_native_queue is None:
                        # Direct mode reached only now (a nested compositor died
                        # or never appeared): arm the compositor callback late.
                        wl_native_queue = self._arm_wayland_native_clipboard()
                    if self.is_wayland and x11_monitor is None:
                        # An X11 desktop's Xwayland comes up after its compositor;
                        # watched from the moment the server answers.
                        x11_monitor = await self._ensure_x11_clipboard_monitor_async()
                    if first_pass:
                        changed = True
                        first_pass = False
                    elif x11_monitor is not None:
                        if not x11_monitor.alive():
                            # Rebuilt in place so outbound clipboard heals on its own
                            # instead of staying dead until a setting is toggled.
                            logger_webrtc_input.warning(
                                "X11 clipboard monitor thread exited; respawning.")
                            try:
                                x11_monitor.close()
                            except Exception:
                                pass
                            self._x11_clipboard_monitor = None
                            x11_monitor = await self._ensure_x11_clipboard_monitor_async()
                            if x11_monitor is None:
                                await asyncio.sleep(0.5)
                                changed = False
                            else:
                                # Republish on the fresh monitor; the baseline still dedupes.
                                changed = True
                        elif wl_native_queue is not None:
                            changed, wl_native_item = await self._wait_x11_or_compositor_change(
                                x11_monitor, wl_native_queue, 2.0)
                        else:
                            changed = await x11_monitor.wait_change(2.0)
                    elif wl_native_queue is not None and not self._has_separate_app_compositor():
                        # Not re-armed on idle: re-registering would stage a full
                        # selection read every tick.
                        try:
                            wl_native_item = await asyncio.wait_for(wl_native_queue.get(), 2.0)
                            changed = True
                        except asyncio.TimeoutError:
                            changed = False
                    elif app_watch_queue is not None:
                        try:
                            await asyncio.wait_for(app_watch_queue.get(), 2.0)
                            changed = True
                        except asyncio.TimeoutError:
                            changed = False
                            # A watch on a dead compositor never fires again; a dead
                            # socket drops the detection so the loop re-arms.
                            self._invalidate_app_wl_display_if_dead(app_watch_display)
                            if self._app_wl_display_cached is None:
                                app_watch_queue, app_watch_display = None, None
                    elif self.is_wayland and self._has_separate_app_compositor():
                        # Poll rung (no data-control watch); the arm above is
                        # retried every tick and takes over the moment one holds.
                        await asyncio.sleep(2.0)
                        changed = True
                    elif self.is_wayland:
                        # Neither watch armed yet (compositor briefly absent).
                        await asyncio.sleep(0.5)
                        changed = False
                    else:
                        # Poll rung; the XFixes rung is re-probed on its cooldown,
                        # so an X server that answers later upgrades back to events.
                        x11_monitor = await self._ensure_x11_clipboard_monitor_async()
                        await asyncio.sleep(0.5)
                        changed = True

                    has_consumers = self._clipboard_has_consumers()
                    if has_consumers and not had_consumers:
                        changed = True
                    had_consumers = has_consumers
                    if not changed:
                        continue
                    if not has_consumers:
                        continue

                    use_binary = self.enable_binary_clipboard in ["true", "out"]
                    if wl_native_item is not None:
                        raw, native_mime = wl_native_item
                        if native_mime.startswith('image/') and use_binary:
                            curr_data, curr_mime = bytes(raw), native_mime
                        elif not native_mime.startswith('image/'):
                            curr_data = bytes(raw).decode('utf-8', errors='replace')
                            curr_mime = 'text/plain'
                        else:
                            curr_data, curr_mime = None, None
                    elif x11_monitor is not None:
                        loop = asyncio.get_running_loop()
                        curr_data, curr_mime = await loop.run_in_executor(
                            None, x11_monitor.read, use_binary)
                    else:
                        curr_data, curr_mime = await self.read_clipboard(use_binary=use_binary)
                    if curr_data is None:
                        curr_data_bytes = None
                    else:
                        curr_data_bytes = curr_data.encode('utf-8') if isinstance(curr_data, str) else curr_data
                    if curr_data_bytes is not None and curr_data_bytes != self._clipboard_last_bytes:
                        logger_webrtc_input.info(f"Clipboard changed. Sending content ({curr_mime})")
                        self._clipboard_last_bytes = curr_data_bytes
                        await self.on_clipboard_read(curr_data, curr_mime)
                except asyncio.CancelledError:
                    logger_webrtc_input.info("Clipboard monitor task cancelled.")
                    break
                except Exception as e:
                    logger_webrtc_input.error(f"Error in clipboard monitor loop: {e}", exc_info=True)
                    await asyncio.sleep(2)
        finally:
            self.clipboard_running = False
            self._clipboard_monitor_active = False
            logger_webrtc_input.info("Clipboard monitor stopped")

    def stop_clipboard(self) -> None:
        """Stop the monitor loop and release the X11 monitor's connection and
        thread; a mode switch builds a fresh handler and must not leak one
        monitor per transition."""
        self.clipboard_running = False
        if self._x11_clipboard_monitor is not None:
            self._x11_clipboard_monitor.close()
            self._x11_clipboard_monitor = None
        logger_webrtc_input.info("Stopping clipboard monitor")


    def _handle_mapping_notify(self, event: Any) -> None:
        """Keep the python-xlib keymap cache and the XTEST overlay coherent with
        server-side keymap changes. Every in-session layout switch (setxkbmap,
        desktop layout applets, fcitx-xkb) lands here as a MappingNotify; without
        the refresh, keysym_to_keycode resolves against the dead layout and the
        overlay trusts bindings the switch wiped.

        A modifier remap only re-resolves the Shift/AltGr keycodes: it never
        touches the overlay, and this shim's own overlay binds surface as
        Modifier notifies on some servers, so clearing the overlay there would
        loop bind, notify, clear forever.
        """
        kb = self.keyboard
        if event.request == X.MappingModifier:
            if kb is not None:
                kb.refresh_modifier_keycodes()
            return
        if event.request != X.MappingKeyboard:
            return
        try:
            self.xdisplay.refresh_keyboard_mapping(event)
        except Exception as e:
            logger_webrtc_input.warning(f"keymap cache refresh failed: {e}")
        if kb is None:
            return
        kb.note_mapping_change(event.first_keycode, event.count)
        if kb.bindings_intact():
            # Our own bind, or a change that left the overlay alone.
            return
        logger_webrtc_input.info(
            "Foreign keymap change detected (request=%d, keycodes %d+%d): "
            "invalidating XTEST overlay state.",
            event.request, event.first_keycode, event.count)
        kb.invalidate_mapping()

    def _dispatch_keymap_event(self, event: Any) -> bool:
        """Hand a keymap change on the input connection to _handle_mapping_notify.

        A core MappingNotify goes as is. A whole-keyboard replacement
        (setxkbmap, a desktop layout switcher) reaches the XKB-aware input
        connection only as XkbNewKeyboardNotify, which is handed on as the
        full-range keyboard MappingNotify it stands for.

        Returns:
            True when the event was a keymap change and has been handled.
        """
        if event.type == X.MappingNotify:
            self._handle_mapping_notify(event)
            return True
        kb = self.keyboard
        span = kb.keyboard_replaced(event) if kb is not None else None
        if span is None:
            return False
        lo, hi = span
        self._handle_mapping_notify(xevent.MappingNotify(
            sequence_number=event.sequence_number, request=X.MappingKeyboard,
            first_keycode=lo, count=hi - lo + 1))
        return True

    async def _keymap_watch_loop(self) -> None:
        """Drain X events for MappingNotify when the cursor monitor is not the
        event consumer (pixelflux delivers cursors natively then). Two consumers
        must never race next_event(), so this loop idles while cursors_running."""
        while True:
            if self.cursors_running or self.xdisplay is None:
                # Idle poll only: the event wake is not armed for this consumer
                # when nothing drives it, and a foreign remap is not urgent here.
                await asyncio.sleep(0.5)
                continue
            wake = self._x_event_wake
            if wake is not None:
                wake.clear()
            if self.xdisplay.pending_events() == 0:
                self._arm_x_event_watcher()
                await self._wait_x_event(timeout=2.0)
            if self.cursors_running or self.xdisplay is None:
                continue
            try:
                while self.xdisplay.pending_events():
                    self._dispatch_keymap_event(self.xdisplay.next_event())
            except asyncio.CancelledError:
                raise
            except Exception as e:
                if self._is_x_conn_closed(e):
                    self._reconnect_xdisplay()
                else:
                    logger_webrtc_input.debug(f"keymap watch: {e}")

    async def start_cursor_monitor(self) -> None:
        """Watch XFixes cursor-change events and push encoded cursors to clients.

        Runs only when pixelflux does not already deliver cursors natively;
        this loop is then the session's single X event consumer (MappingNotify
        included), so it never races _keymap_watch_loop on next_event(). The
        X fetch stays on this thread (python-xlib connections are not
        thread-safe and the loop also injects input on this display), while
        the PIL resize and PNG encode, pure CPU, run off the loop.
        """
        if self.is_wayland:
            logger_webrtc_input.info("Wayland mode: Cursor monitor disabled (handled by compositor callback).")
            return
        if pixelflux_x11_cursor():
            logger_webrtc_input.info(
                "X11 cursor monitor disabled (pixelflux cursor callback active)."
            )
            return
        if not self.xdisplay.has_extension("XFIXES"):
            if self.xdisplay.query_extension("XFIXES") is None:
                logger_webrtc_input.error(
                    "XFIXES extension not supported, cannot watch cursor changes"
                )
                return
        xfixes_version = self.xdisplay.xfixes_query_version()
        logger_webrtc_input.info(
            "Found XFIXES version %s.%s",
            xfixes_version.major_version,
            xfixes_version.minor_version,
        )
        logger_webrtc_input.info("starting cursor monitor")
        self.cursors_running = True
        screen = self.xdisplay.screen()
        self.xdisplay.xfixes_select_cursor_input(
            screen.root, xfixes.XFixesDisplayCursorNotifyMask
        )
        logger_webrtc_input.info("watching for cursor changes")
        try:
            cursor_image = self.xdisplay.xfixes_get_cursor_image(screen.root)
            cursor_data = await asyncio.to_thread(self._encode_cursor, cursor_image)
            self.on_cursor_change(cursor_data)
        except Exception as e:
            logger_webrtc_input.warning("exception from fetching initial cursor image: %s", e)
            if self._is_x_conn_closed(e):
                self._reconnect_xdisplay()

        while self.cursors_running:
            if self.xdisplay is None:
                # A background reconnect is in flight; the fresh connection
                # was xfixes-armed at install, so only screen needs rebinding.
                await asyncio.sleep(0.5)
                if self.xdisplay is not None:
                    screen = self.xdisplay.screen()
                continue
            wake = self._x_event_wake
            if wake is not None:
                wake.clear()
            if self.xdisplay.pending_events() == 0:
                # The 1 s failsafe bounds the wait if the loop reader was lost to a reconnect.
                self._arm_x_event_watcher()
                await self._wait_x_event(timeout=1.0)
                continue

            event = self.xdisplay.next_event()
            if self._dispatch_keymap_event(event):
                continue
            if (event.type, 0) == self.xdisplay.extension_event.DisplayCursorNotify:
                try:
                    cursor_image = self.xdisplay.xfixes_get_cursor_image(screen.root)
                    cursor_data = await asyncio.to_thread(self._encode_cursor, cursor_image)
                    self.on_cursor_change(cursor_data)
                except Exception as e:
                    logger_webrtc_input.warning(
                        "exception from fetching cursor image on change: %s", e
                    )
                    if self._is_x_conn_closed(e):
                        # The None guard above rebinds screen once the reconnect lands.
                        self._reconnect_xdisplay()
        logger_webrtc_input.info("cursor monitor stopped")

    def stop_cursor_monitor(self) -> None:
        logger_webrtc_input.info("stopping cursor monitor")
        self.cursors_running = False

    def get_current_cursor_data(self) -> Optional[dict]:
        """One-shot fetch of the current X cursor as a client message, for
        seeding a newly connected client; None when unavailable."""
        if self.is_wayland:
            return None
        if not self.enable_cursors or not self.xdisplay:
            return None
        try:
            if not self.xdisplay.has_extension("XFIXES"):
                if self.xdisplay.query_extension("XFIXES") is None:
                    logger_webrtc_input.error(
                        "XFIXES extension not supported, cannot fetch current cursor"
                    )
                    return None
            # XFixes wants version negotiation before any other request, and
            # this fetch runs whether or not the cursor monitor is up.
            if not getattr(self, "_xfixes_negotiated", False):
                self.xdisplay.xfixes_query_version()
                self._xfixes_negotiated = True
            screen = self.xdisplay.screen()
            cursor_image = self.xdisplay.xfixes_get_cursor_image(screen.root)
            return self._encode_cursor(cursor_image)
        except Exception as e:
            logger_webrtc_input.warning("exception from fetching current cursor image: %s", e)
            return None

    def _encode_cursor(self, cursor: Any) -> dict:
        """cursor_to_msg behind a one-entry cache keyed by the XFixes cursor
        serial (and the size cap the encode depends on): the monitor encodes
        each cursor once, off the loop, and the per-connect fetch — sync, on
        the loop — reuses that instead of encoding the same PNG again."""
        key = (getattr(cursor, "cursor_serial", None), self.cursor_size_cap)
        cached = self._cursor_msg_cache
        if key[0] is not None and cached is not None and cached[0] == key:
            return cached[1]
        msg = self.cursor_to_msg(cursor)
        if key[0] is not None:
            self._cursor_msg_cache = (key, msg)
        return msg

    def _cursor_image_to_pil(self, cursor: Any) -> Image.Image:
        byte_data = b''.join(p.to_bytes(4, 'little') for p in cursor.cursor_image)
        return Image.frombytes("RGBA", (cursor.width, cursor.height), byte_data, "raw", "BGRA")

    def cursor_to_msg(self, cursor: Any) -> dict:
        """Encode an XFixes cursor image into the client cursor message.

        Crops to the visible bounding box (clamping the hotspot with it),
        resizes down to the DPI-scaled cap, un-premultiplies alpha, and
        base64-encodes a PNG. Pure CPU work — callers on the event loop run it
        via a thread. XFixes pixels are premultiplied and are straightened
        only after any resize (resampling is linear per channel in
        premultiplied space), matching the pixelflux monitor's pipeline; the
        handle shares format_pixelflux_cursor's pixel-content space so the
        connect-time seed and the live path dedupe one shape to one client
        cache entry.
        """
        if not cursor or cursor.width == 0 or cursor.height == 0:
            return {
                "curdata": "", "width": 0, "height": 0,
                "hotx": 0, "hoty": 0, "handle": 0,
            }
        im = self._cursor_image_to_pil(cursor)
        bbox = im.getbbox()
        if bbox is None:
            return {
                "curdata": "", "width": 0, "height": 0,
                "hotx": 0, "hoty": 0, "handle": 0,
            }
        cropped_im = im.crop(bbox)
        left, upper, right, lower = bbox
        # Browsers clamp a negative CSS cursor hotspot to 0 silently; clamping
        # here, at the crop rebase, keeps both renderers agreeing.
        new_hotx = max(0, cursor.xhot - left)
        new_hoty = max(0, cursor.yhot - upper)
        if cropped_im.width > self.cursor_size_cap or cropped_im.height > self.cursor_size_cap:
            if self.cursor_debug:
                logger_webrtc_input.info(f"Cursor ({cropped_im.width}x{cropped_im.height}) exceeds cap ({self.cursor_size_cap}x{self.cursor_size_cap}). Resizing.")
            max_dim = max(cropped_im.width, cropped_im.height)
            scale_factor = self.cursor_size_cap / max_dim
            new_width = int(cropped_im.width * scale_factor)
            new_height = int(cropped_im.height * scale_factor)
            cropped_im = cropped_im.resize(
                (new_width, new_height), resample=Image.Resampling.LANCZOS
            )
            new_hotx = min(round(new_hotx * scale_factor), max(0, new_width - 1))
            new_hoty = min(round(new_hoty * scale_factor), max(0, new_height - 1))
        # After the resize, never before (see the docstring).
        cropped_im = unpremultiply_rgba(cropped_im)
        with io.BytesIO() as f:
            cropped_im.save(f, "PNG")
            png_data = f.getvalue()
        png_data_b64 = base64.b64encode(png_data)
        return {
            "curdata": png_data_b64.decode(),
            "width": cropped_im.width,
            "height": cropped_im.height,
            "hotx": new_hotx,
            "hoty": new_hoty,
            "handle": cursor_content_handle(
                cropped_im.tobytes(), cropped_im.width, cropped_im.height,
                new_hotx, new_hoty),
        }

    async def stop_gamepad_servers(self) -> None:
        logger_webrtc_input.info("Stopping all gamepad instances.")
        await self.__gamepad_disconnect()

    def _keyboard_enqueue(self, item: tuple) -> None:
        """Enqueue input for the keyboard worker, evicting the oldest entry on
        overflow so a message flood can't grow the queue without bound. A held key
        orphaned by an evicted release is recovered by the stale sweep, which only
        covers keysyms tracked in pressed_keys — server-generated sequences are not
        tracked, so they go through _keyboard_enqueue_chord instead."""
        try:
            self.keyboard_queue.put_nowait(item)
        except asyncio.QueueFull:
            try:
                evicted = self.keyboard_queue.get_nowait()
                self.keyboard_queue.task_done()
                # A queued server-side reset is awaited; its waiter must not
                # sit out the timeout because a flood evicted it.
                if evicted[0] == "kr" and evicted[1] is not None and not evicted[1].done():
                    evicted[1].set_result(None)
            except asyncio.QueueEmpty:
                pass
            try:
                self.keyboard_queue.put_nowait(item)
            except asyncio.QueueFull:
                logger_webrtc_input.warning("keyboard queue full; dropping input event.")

    def _keyboard_enqueue_chord(self, keys: Iterable[tuple]) -> None:
        """Enqueue a server-synthesized press/release sequence as ONE entry, so
        overflow eviction can only lose it whole.

        `keys` is a sequence of (keysym, down) pairs the worker injects in order.
        These keysyms never enter pressed_keys (no client 'ku' follows them), so a
        release evicted on its own would leave the modifier held with nothing to
        heal it — the stale sweep only covers tracked keys.
        """
        self._keyboard_enqueue(("chord", tuple(keys)))

    def _route_key_as_text(self, keysym: int) -> None:
        """Record a keysym whose kd became buffered text, so its ku is swallowed
        instead of releasing a key that was never pressed. Bounded like
        pressed_keys, evicting the oldest entry."""
        if len(self._wl_text_routed) >= self.max_pressed_keys:
            self._wl_text_routed.pop(next(iter(self._wl_text_routed)), None)
        self._wl_text_routed[keysym] = True

    async def _keyboard_worker(self) -> None:
        """Wayland's single serialized key-injection loop.

        Drains keyboard_queue so every rung (seat keymap, virtual-keyboard
        batch, clipboard paste) sees keys in client order. Character-bearing
        keysyms the seat cannot deliver — Unicode-plane keysyms off the seat,
        and legacy-plane keysyms the base layout lacks when a nested app
        compositor would re-translate a seat overlay bind — accumulate in a
        text buffer that flushes as one batch to the app compositor; the flush
        happens before any directly injected key so ordering still holds.

        Chord-translated Cyrillic stays on the seat, where its QWERTY keysym
        resolves under any latin-based keymap; control and navigation keysyms
        spell no character, so the owner is never consulted for them. A
        modifier-held Unicode keysym normally goes through the seat so the app
        sees the chord, but under a nested app compositor, which never
        resolves seat-overlay keysyms, the text is typed plain rather than
        vanishing.
        """
        unicode_buffer = []

        def native_inject():
            """Whether the keymap owner can deliver any keysym in order: only when
            the capture compositor is also where the apps run, since a nested app
            compositor never sees its overlay binds."""
            return (bool(self.wayland_input
                         and hasattr(self.wayland_input, 'set_keymap_string'))
                    and not self._has_separate_app_compositor())

        async def flush_buffer():
            """Type the buffered text (non-layout keysyms and IME strings) through
            zwp_virtual_keyboard, or paste it via the clipboard on a compositor
            without that protocol (KWin); never through the keymap overlay."""
            if unicode_buffer:
                combined_text = "".join(unicode_buffer)
                unicode_buffer.clear()

                try:
                    await self._wl_type_text(combined_text)
                    return
                except Exception as e:
                    logger_webrtc_input.debug(
                        f"virtual-keyboard batch failed ({e}); pasting via clipboard")
                if not await self._inject_text_via_clipboard(combined_text):
                    logger_webrtc_input.warning(
                        f"Batched text injection failed; {len(combined_text)} chars dropped.")

        while True:
            try:
                if unicode_buffer:
                    try:
                        msg_type, data = await asyncio.wait_for(self.keyboard_queue.get(), timeout=0.05)
                    except asyncio.TimeoutError:
                        await flush_buffer()
                        continue
                else:
                    msg_type, data = await self.keyboard_queue.get()

                try:
                    keysym = data if msg_type in ("kd", "ku") else None
                    is_unicode_fallback = False
                    if keysym is not None:
                        is_unicode_fallback = (0xA0 <= keysym <= 0xFF) or keysym == 0x20AC or ((keysym & 0xFF000000) == 0x01000000)

                    if msg_type == "kd":
                        if (is_unicode_fallback and not native_inject()
                                and (not self.active_modifiers
                                     or self._has_separate_app_compositor())):
                            unicode_codepoint = keysym & 0x00FFFFFF if (keysym & 0xFF000000) == 0x01000000 else keysym
                            try:
                                char_to_type = chr(unicode_codepoint)
                                unicode_buffer.append(char_to_type)
                                self._route_key_as_text(keysym)
                                continue
                            except ValueError:
                                pass

                        if keysym == 65288 and unicode_buffer:
                            unicode_buffer.pop()
                            continue

                        if (not is_unicode_fallback
                                and keysym is not None and not native_inject()
                                and keysym not in self.MODIFIER_KEYSYMS
                                and not ((self.active_modifiers & self.ACTION_MODIFIER_KEYSYMS)
                                         and keysym in CYRILLIC_TO_QWERTY_KEYSYM)):
                            char_to_type = keysym_to_character(keysym)
                            if char_to_type is not None:
                                owner = await self._ensure_wayland_keymap_owner()
                                if owner is None or not owner.resolves(keysym):
                                    unicode_buffer.append(char_to_type)
                                    self._route_key_as_text(keysym)
                                    continue

                        await flush_buffer()

                        if keysym in self.MODIFIER_KEYSYMS:
                            self.active_modifiers.add(keysym)

                        await self.send_x11_keypress(keysym, down=True)

                    elif msg_type == "ku":
                        if keysym is not None and self._wl_text_routed.pop(keysym, None):
                            # Its kd became text, whatever the topology is by now.
                            continue
                        if is_unicode_fallback and not native_inject():
                            # Buffered text was typed atomically; no held key.
                            continue

                        if keysym in self.MODIFIER_KEYSYMS:
                            self.active_modifiers.discard(keysym)
                        if keysym in self.atomically_typed_keys:
                            self.atomically_typed_keys.discard(keysym)
                        else:
                            await self.send_x11_keypress(keysym, down=False)

                    elif msg_type == "chord":
                        # Back-to-back, so no other queued key lands inside the chord.
                        await flush_buffer()
                        for chord_keysym, down in data:
                            if chord_keysym in self.MODIFIER_KEYSYMS:
                                if down:
                                    self.active_modifiers.add(chord_keysym)
                                else:
                                    self.active_modifiers.discard(chord_keysym)
                            await self.send_x11_keypress(chord_keysym, down=down)

                    elif msg_type == "kr":
                        # data is the future a server-side reset awaits; None from a client.
                        self._wl_text_routed.clear()
                        await flush_buffer()
                        try:
                            await self._reset_keyboard_wayland()
                        finally:
                            if data is not None and not data.done():
                                data.set_result(None)

                    elif msg_type == "co_end":
                        if native_inject():
                            # One keymap swap binds every missing keysym; per-char is the fallback.
                            typed = False
                            owner = await self._ensure_wayland_keymap_owner()
                            if owner is not None:
                                try:
                                    typed = await asyncio.to_thread(
                                        owner.type_text, data,
                                        not (self.active_modifiers
                                             & self.ACTION_MODIFIER_KEYSYMS))
                                except Exception as e:
                                    logger_webrtc_input.warning(
                                        f"Batched Wayland composition type failed; "
                                        f"falling back per-char: {e}")
                            if not typed:
                                for ch in data:
                                    cp = ord(ch)
                                    ks = cp if 0x20 <= cp <= 0xFF else (0x01000000 | cp)
                                    await self.send_x11_keypress(ks, down=True)
                                    await self.send_x11_keypress(ks, down=False)
                        else:
                            unicode_buffer.append(data)

                finally:
                    self.keyboard_queue.task_done()

            except asyncio.CancelledError:
                break
            except Exception as e:
                logger_webrtc_input.error(f"Error in keyboard worker: {e}", exc_info=True)

    def _reset_multipart_clipboard(self) -> None:
        """Reset all multi-part clipboard receive state to its idle defaults.

        Used on completion/abort so no field (size, mime type, buffer, id, kind)
        is left stale to bleed into the next cbs/cws transfer.
        """
        self.multipart_clipboard_buffer = None
        self.multipart_clipboard_in_progress = False
        self.multipart_clipboard_id = None
        self.multipart_clipboard_kind = None
        self.multipart_clipboard_total_size = 0
        self.multipart_clipboard_mime_type = "text/plain"

    async def on_message(self, msg: str, display_id: str = 'primary',
                         conn_id: Any = None) -> None:
        """Transport entry point for one client message.

        A malformed client message must not tear down the transport
        connection, so parse errors are logged and swallowed here.

        Args:
            msg: Raw comma-delimited message string.
            display_id: Transport-level id of the display whose channel
                delivered the message (not a spoofable payload field).
            conn_id: Transport connection identity, for per-connection state
                (gamepad associations, clipboard debounce).
        """
        try:
            await self._dispatch_message(msg, display_id, conn_id)
        except (IndexError, ValueError) as e:
            logger_webrtc_input.warning(f"Malformed client message {msg[:64]!r}: {e}")
        except Exception as e:
            logger_webrtc_input.error(f"Error handling client message {msg[:64]!r}: {e}", exc_info=True)

    async def _dispatch_message(self, msg: str, display_id: str = 'primary',
                                conn_id: Any = None) -> None:
        """Parse and act on one client message (the whole wire protocol lives here)."""
        toks = msg.split(",")
        msg_type = toks[0]

        if msg_type == "pong":
            if self.ping_start is None:
                # A straggler from the other mode's client after a transport flip
                # (WebRTC pings over its data channel; WS has no app-level ping).
                logger_webrtc_input.debug("received pong before ping; ignoring")
                return
            self.on_ping_response(float("%.3f" % ((time.time() - self.ping_start) / 2 * 1000)))
        elif msg_type == "kd":
            keysym = int(toks[1])
            # At the cap the oldest entry goes, not the new one: the key is
            # injected below regardless, and untracked it would never auto-release.
            if keysym in self.pressed_keys:
                self.pressed_keys[keysym] = time.monotonic()
            else:
                if len(self.pressed_keys) >= self.max_pressed_keys:
                    oldest_keysym = min(self.pressed_keys, key=self.pressed_keys.get)
                    self.pressed_keys.pop(oldest_keysym, None)
                self.pressed_keys[keysym] = time.monotonic()
            self.reaped_atomic_keys.discard(keysym)
            if self.is_wayland:
                self._keyboard_enqueue(("kd", keysym))
            else:
                is_printable = (0x20 <= keysym <= 0xFF) or ((keysym & 0xFF000000) == 0x01000000)
                if keysym in self.MODIFIER_KEYSYMS:
                    self.active_modifiers.add(keysym)
                if is_printable and not self.active_modifiers:
                    unicode_codepoint = keysym & 0x00FFFFFF if (keysym & 0xFF000000) == 0x01000000 else keysym
                    try:
                        char_to_type = chr(unicode_codepoint)
                        if not char_to_type.isalpha() and char_to_type != ' ':
                            await self.on_message(f"co,end,{char_to_type}")
                            self.atomically_typed_keys.add(keysym)
                        else:
                            await self.send_x11_keypress(keysym, down=True)
                    except (ValueError, TypeError):
                        await self.send_x11_keypress(keysym, down=True)
                else:
                    await self.send_x11_keypress(keysym, down=True)
                # Arm auto-repeat: pop+insert moves the key to the end (only the
                # newest repeats); modifiers never; atomic keys are armed too.
                if (self.key_repeat_enabled and keysym not in self.MODIFIER_KEYSYMS):
                    self.key_repeat_state.pop(keysym, None)
                    self.key_repeat_state[keysym] = time.monotonic() + self.key_repeat_delay
                else:
                    self.key_repeat_state.pop(keysym, None)
        elif msg_type == "ku":
            keysym = int(toks[1])
            self.pressed_keys.pop(keysym, None)
            self.key_repeat_state.pop(keysym, None)
            if self.is_wayland:
                self._keyboard_enqueue(("ku", keysym))
            else:
                if keysym in self.MODIFIER_KEYSYMS:
                    self.active_modifiers.discard(keysym)
                if keysym in self.reaped_atomic_keys:
                    # Already reaped by the sweep; a keyup now would be spurious.
                    self.reaped_atomic_keys.discard(keysym)
                elif keysym in self.atomically_typed_keys:
                    # Never physically held on X11; nothing to release.
                    self.atomically_typed_keys.discard(keysym)
                else:
                    await self.send_x11_keypress(keysym, down=False)
        elif msg_type == "kr":
            if self.is_wayland:
                self._keyboard_enqueue(("kr", None))
            else:
                await self.reset_keyboard()
        elif msg_type == "kh":
            # Refresh only, no injection. Atomic keys are refreshed like any
            # other: the repeat loop pauses on a stale heartbeat, so skipping
            # them would kill their auto-repeat before the first repeat is due.
            now = time.monotonic()
            # Bounded: a client could otherwise pack one frame with tens of
            # thousands of tokens.
            for tok in toks[1:1 + self.max_pressed_keys]:
                try:
                    keysym = int(tok)
                except ValueError:
                    continue
                if keysym in self.pressed_keys:
                    self.pressed_keys[keysym] = now
        elif msg_type in ["m", "m2"]:
            relative = msg_type == "m2"
            # Dropped rather than defaulted: a default would warp to the origin.
            try: x, y, button_mask, scroll_magnitude = [int(i) for i in toks[1:]]
            except (ValueError, IndexError): return
            try: await self.send_x11_mouse(x, y, button_mask, scroll_magnitude, relative, display_id=display_id)
            except Exception as e: logger_webrtc_input.warning(f"Failed to set mouse cursor: {e}")
        elif msg_type == "p": await self.on_mouse_pointer_visible(bool(int(toks[1])))
        elif msg_type == "vb":
            try:
                # kbps; per display, named by the delivering channel.
                bitrate = int(toks[1])
                if bitrate <= 0:
                    return
                await self.on_video_encoder_bit_rate(bitrate, display_id)
            except Exception as e:
                logger_webrtc_input.error(f"Error video bitrate change: {e}")
        elif msg_type == "ab":
            try:
                bitrate = int(toks[1])
                if bitrate <= 0:
                    return
                await self.on_audio_encoder_bit_rate(bitrate)
            except Exception as e:
                logger_webrtc_input.error(f"Error audio bitrate change: {e}")
        elif msg_type == "js":
            # Enforced server-side so a client cannot inject controller input
            # whatever its own UI state.
            if not settings.gamepad_enabled[0]:
                return
            cmd = toks[1]
            gamepad_idx = int(toks[2])

            if not (0 <= gamepad_idx < self.num_gamepads):
                logger_webrtc_input.error(f"Client message for gamepad index {gamepad_idx} is out of range (0-{self.num_gamepads-1}).")
                return

            target_gamepad_instance = self.gamepad_instances.get(gamepad_idx)
            if not target_gamepad_instance:
                logger_webrtc_input.error(
                    f"CRITICAL: No persistent SelkiesGamepad instance found for index {gamepad_idx} in on_message. "
                    f"Gamepad system may not be initialized correctly."
                )
                return

            if cmd == "c": 
                try: client_name_decoded = base64.b64decode(toks[3]).decode('latin-1', 'ignore')[:255]
                except Exception as e: client_name_decoded = f"ClientGamepad{gamepad_idx}"; logger_webrtc_input.warning(f"Error decoding client gamepad name: {e}")
                client_num_axes, client_num_btns = int(toks[4]), int(toks[5])
                
                await self.__gamepad_connect(gamepad_idx, client_name_decoded, client_num_btns, client_num_axes, conn_id=conn_id)

            elif cmd == "d": 
                await self.__gamepad_disconnect(gamepad_idx)
            
            elif cmd == "b": 
                button_num = int(toks[3])
                button_val = float(toks[4])
                target_gamepad_instance.send_event(button_num, button_val, is_button_event=True)

            elif cmd == "a":
                axis_num = int(toks[3])
                axis_val = float(toks[4])
                target_gamepad_instance.send_event(axis_num, axis_val, is_button_event=False)

            elif cmd == "h":
                # Held-state heartbeat: refresh only (no injection), like 'kh'.
                self.gamepad_heartbeats[gamepad_idx] = time.monotonic()

            else: logger_webrtc_input.warning(f"Unhandled joystick command for slot {gamepad_idx}: js {cmd}")
        elif msg_type == "cws":
            if self.enable_clipboard in ["true", "in"]:
                try:
                    transfer_id = toks[1]
                    declared_size = int(toks[2])
                    if declared_size < 0 or declared_size > MULTIPART_CLIPBOARD_MAX_SIZE:
                        logger_webrtc_input.error(f"Rejecting multi-part clipboard write: declared size {declared_size} out of bounds (max {MULTIPART_CLIPBOARD_MAX_SIZE}).")
                        return
                    if self.multipart_clipboard_in_progress and transfer_id != self.multipart_clipboard_id:
                        logger_webrtc_input.warning(f"Aborting previous in-progress clipboard transfer {self.multipart_clipboard_id} for new transfer {transfer_id}.")
                    self.multipart_clipboard_id = transfer_id
                    self.multipart_clipboard_kind = "text"
                    self.multipart_clipboard_total_size = declared_size
                    self.multipart_clipboard_mime_type = "text/plain"
                    self.multipart_clipboard_buffer = io.BytesIO()
                    self.multipart_clipboard_in_progress = True
                    logger_webrtc_input.info(f"Starting multi-part text clipboard receive, total size: {self.multipart_clipboard_total_size}")
                except Exception as e:
                    logger_webrtc_input.error(f"Invalid cws message: {msg}, error: {e}")
            else:
                logger_webrtc_input.warning("Rejecting multi-part clipboard write: inbound clipboard disabled.")
        elif msg_type == "cbs":
            # Direction gate and binary gate: the server enforces its own policy.
            if self.enable_clipboard in ["true", "in"] and self.enable_binary_clipboard in ["true", "in"]:
                try:
                    transfer_id = toks[1]
                    declared_size = int(toks[3])
                    if declared_size < 0 or declared_size > MULTIPART_CLIPBOARD_MAX_SIZE:
                        logger_webrtc_input.error(f"Rejecting multi-part clipboard write: declared size {declared_size} out of bounds (max {MULTIPART_CLIPBOARD_MAX_SIZE}).")
                        return
                    if self.multipart_clipboard_in_progress and transfer_id != self.multipart_clipboard_id:
                        logger_webrtc_input.warning(f"Aborting previous in-progress clipboard transfer {self.multipart_clipboard_id} for new transfer {transfer_id}.")
                    self.multipart_clipboard_id = transfer_id
                    self.multipart_clipboard_kind = "binary"
                    self.multipart_clipboard_mime_type = toks[2]
                    self.multipart_clipboard_total_size = declared_size
                    self.multipart_clipboard_buffer = io.BytesIO()
                    self.multipart_clipboard_in_progress = True
                    logger_webrtc_input.info(f"Starting multi-part binary clipboard receive ({self.multipart_clipboard_mime_type}), total size: {self.multipart_clipboard_total_size}")
                except Exception as e:
                    logger_webrtc_input.error(f"Invalid cbs message: {msg}, error: {e}")
            else:
                logger_webrtc_input.warning("Rejecting multi-part clipboard write: inbound clipboard disabled.")
        elif msg_type == "cwd" or msg_type == "cbd":
            expected_kind = "text" if msg_type == "cwd" else "binary"
            # Token count first: a malformed chunk raising mid-transfer would
            # leave the multipart state half-open, accumulating until overflow.
            if len(toks) < 3:
                logger_webrtc_input.warning(f"Malformed clipboard chunk ({msg_type}): missing fields; aborting transfer.")
                self._reset_multipart_clipboard()
            elif not (self.multipart_clipboard_in_progress and toks[1] == self.multipart_clipboard_id and self.multipart_clipboard_kind == expected_kind):
                logger_webrtc_input.warning(f"Ignoring mismatched clipboard chunk ({msg_type}): id/kind does not match active transfer.")
            else:
                try:
                    chunk_data = base64.b64decode(toks[2])
                    if self.multipart_clipboard_buffer.tell() + len(chunk_data) > self.multipart_clipboard_total_size:
                        logger_webrtc_input.error("Multi-part clipboard exceeded its declared size; aborting transfer.")
                        self._reset_multipart_clipboard()
                        return
                    self.multipart_clipboard_buffer.write(chunk_data)
                except Exception as e:
                    logger_webrtc_input.error(f"Failed to process clipboard data chunk: {e}")
                    self._reset_multipart_clipboard()
        elif msg_type == "cwe" or msg_type == "cbe":
            expected_kind = "text" if msg_type == "cwe" else "binary"
            # Token count first, so a malformed end does not raise mid-state.
            if len(toks) < 2:
                logger_webrtc_input.warning(f"Malformed clipboard end ({msg_type}): missing id; aborting transfer.")
                self._reset_multipart_clipboard()
            elif not (self.multipart_clipboard_in_progress and toks[1] == self.multipart_clipboard_id and self.multipart_clipboard_kind == expected_kind):
                logger_webrtc_input.warning(f"Ignoring mismatched clipboard end ({msg_type}): id/kind does not match active transfer.")
            else:
                received_size = self.multipart_clipboard_buffer.tell()
                if received_size != self.multipart_clipboard_total_size:
                    logger_webrtc_input.error(f"Multi-part clipboard size mismatch. Expected {self.multipart_clipboard_total_size}, got {received_size}. Aborting.")
                else:
                    logger_webrtc_input.info(f"Finished multi-part clipboard receive. Total size: {received_size}")
                    data = self.multipart_clipboard_buffer.getvalue()
                    mime_type = self.multipart_clipboard_mime_type
                    # Awaited in-line: a paste keystroke right behind the transfer
                    # must find the clipboard set. Bytes pass straight through; a
                    # multi-MB decode and re-encode on the loop would be redundant.
                    if await self.write_clipboard(data, mime_type=mime_type):
                        if mime_type == "text/plain":
                            logger_webrtc_input.info(f"Set multi-part clipboard content, length: {len(data)}")
                        else:
                            logger_webrtc_input.info(f"Set multi-part binary clipboard content ({mime_type}), size: {len(data)} bytes")
                self._reset_multipart_clipboard()
        elif msg_type == "cr":
            if self.enable_clipboard in ["true", "out"]:
                data, mime_type = await self.read_clipboard(use_binary=self.enable_binary_clipboard in ["true", "out"])
                if data:
                    # Tagged (reply_to) so the client treats it cache-only without
                    # its connect-time 5 s heuristic, and sent to this client alone:
                    # unasked-for, another client would cache content it never pastes.
                    await self.send_clipboard_data(
                        data, mime_type, reply_to="cr", conn_id=conn_id)
                else:
                    # Reply even when empty: the tag settles the client's connect-time
                    # fetch, so a real change seconds later is not taken for the snapshot.
                    logger_webrtc_input.debug("No clipboard content; sending empty tagged reply")
                    await self.send_clipboard_data(
                        "", "text/plain", reply_to="cr", conn_id=conn_id)
            else: logger_webrtc_input.warning("Rejecting clipboard read: outbound clipboard disabled.")
        elif msg_type == "REQUEST_CLIPBOARD":
            if self.enable_clipboard in ["true", "out"]:
                now = time.monotonic()
                # display_id stands in when no per-connection id is supplied.
                clip_key = conn_id if conn_id is not None else display_id
                # Bounded across reconnecting connections.
                if len(self._last_clipboard_request_ts) > 64:
                    self._last_clipboard_request_ts = {
                        k: ts for k, ts in self._last_clipboard_request_ts.items()
                        if now - ts < self._clipboard_request_debounce
                    }
                if now - self._last_clipboard_request_ts.get(clip_key, 0.0) < self._clipboard_request_debounce:
                    logger_webrtc_input.debug("Debouncing REQUEST_CLIPBOARD (too frequent).")
                else:
                    self._last_clipboard_request_ts[clip_key] = now
                    use_binary = self.enable_binary_clipboard in ["true", "out"]
                    async def _send_requested_clipboard():
                        """Read and push the clipboard as a task, so a slow read
                        cannot stall the dispatch loop. A read that still matches
                        the baseline races the injected Ctrl+C (the app has not
                        published the new selection yet), so it waits briefly for
                        the owner change — without consuming the edge the monitor
                        loop broadcasts on — and re-reads."""
                        try:
                            data, mime_type = await self.read_clipboard(use_binary=use_binary)
                            data_bytes = (data.encode('utf-8')
                                          if isinstance(data, str) else data)
                            if data_bytes is not None and data_bytes == self._clipboard_last_bytes:
                                monitor = self._x11_clipboard_monitor
                                if monitor is not None and monitor.alive():
                                    await monitor.peek_change(0.15)
                                else:
                                    await asyncio.sleep(0.15)
                                data, mime_type = await self.read_clipboard(use_binary=use_binary)
                            if data:
                                await self.send_clipboard_data(data, mime_type,
                                                               conn_id=conn_id)
                            else:
                                logger_webrtc_input.debug("No clipboard content to send on REQUEST_CLIPBOARD.")
                        except Exception as e:
                            logger_webrtc_input.warning(f"REQUEST_CLIPBOARD read failed: {e}")
                    self._spawn_task(_send_requested_clipboard())
            else:
                logger_webrtc_input.warning("Rejecting REQUEST_CLIPBOARD: outbound clipboard disabled.")
        elif msg_type == "cb":
            # Same double gate as cbs.
            if self.enable_clipboard in ["true", "in"] and self.enable_binary_clipboard in ["true", "in"]:
                try:
                    _, mime_type, b64_data = toks
                    data_bytes = base64.b64decode(b64_data)
                    # In-line so a paste keystroke right behind it pastes this content.
                    if await self.write_clipboard(data_bytes, mime_type=mime_type):
                        logger_webrtc_input.info(f"Set binary clipboard content ({mime_type}), size: {len(data_bytes)} bytes")
                except Exception as e:
                    logger_webrtc_input.error(f"Binary clipboard write error: {e}")
            else:
                logger_webrtc_input.warning("Rejecting binary clipboard write: inbound binary clipboard disabled.")
        elif msg_type == "cw": 
            if self.enable_clipboard in ["true", "in"]:
                try:
                    data = base64.b64decode(toks[1]).decode("utf-8", 'ignore')
                    # In-line for paste-after-copy ordering (see the cb branch).
                    if await self.write_clipboard(data):
                        logger_webrtc_input.info(f"Set clipboard content, length: {len(data)}")
                except Exception as e:
                    logger_webrtc_input.error(f"Clipboard decode error: {e}")
                    return
            else: 
                logger_webrtc_input.warning("Rejecting clipboard write: inbound clipboard disabled.")
        elif msg_type == "r":
            res = toks[1]
            if re.fullmatch(r"^\d+x\d+$", res):
                # Passed through verbatim: even-dim normalization lives in
                # parse_resize_dims so both transports realize it identically.
                _r = self.on_resize(res, display_id)
                if asyncio.iscoroutine(_r): await _r
            else: logger_webrtc_input.warning(f"Rejecting resolution change, invalid: {res}")
        elif msg_type == "s":
            scale = toks[1]
            if re.fullmatch(r"^\d+(\.\d+)?$", scale):
                _s = self.on_scaling_ratio(float(scale))
                if asyncio.iscoroutine(_s): await _s
            else: logger_webrtc_input.warning(f"Rejecting scaling change, invalid: {scale}")
        elif msg_type == "cmd":
            if not settings.command_enabled[0]:
                logger_webrtc_input.warning("Received 'cmd' message, but command execution is disabled by server settings.")
                return
            if len(toks) > 1:
                command_to_run = ",".join(toks[1:])
                logger_webrtc_input.info(f"Attempting to execute command: '{command_to_run}'")

                async def _notify_cmd_error(text, conn_id=conn_id):
                    self.send_command_error(text, conn_id)

                await run_client_command(
                    command_to_run, logger_webrtc_input, notify=_notify_cmd_error,
                    env=self.app_launch_env())
            else:
                logger_webrtc_input.warning("Received 'cmd' message without a command string.")
        elif msg_type == "_arg_fps":
            try:
                fps = int(toks[1])
                if fps <= 0:
                    return
                await self.on_set_fps(fps, display_id)
            except Exception as e:
                logger_webrtc_input.error(f"Error fps change: {e}")
        elif msg_type == "_arg_resize":
            if len(toks) == 3:
                enabled, res_str = toks[1].lower() == "true", toks[2]
                enable_res = None
                if re.fullmatch(r"^\d+x\d+$", res_str):
                    w,h = [int(i)+int(i)%2 for i in res_str.split("x")]; enable_res = f"{w}x{h}"
                elif res_str: logger_webrtc_input.warning(f"Invalid resolution for enable_resize: {res_str}")
                self.on_set_enable_resize(enabled, enable_res)
            else: logger_webrtc_input.error("Invalid _arg_resize command format")
        elif msg_type == "_f": 
            try: self.on_client_fps(int(toks[1]))
            except (ValueError, IndexError): logger_webrtc_input.error(f"Failed to parse client FPS: {toks}")
        elif msg_type == "_l": 
            try: self.on_client_latency(int(toks[1]))
            except (ValueError, IndexError): logger_webrtc_input.error(f"Failed to parse client latency: {toks}")
        elif msg_type in ["_stats_video", "_stats_audio"]: 
            try: await self.on_client_webrtc_stats(msg_type, ",".join(toks[1:]))
            except (ValueError, IndexError): logger_webrtc_input.error("Failed to parse WebRTC Statistics")
        elif msg_type == "co" and toks[1] == "end":
            try:
                text_to_type = msg[7:]
                if self.is_wayland:
                    self._keyboard_enqueue(("co_end", text_to_type))
                elif self._type_text_xtest(
                        text_to_type,
                        neutralize=not (self.active_modifiers
                                        & self.ACTION_MODIFIER_KEYSYMS)):
                    # Typed in-process; xdotool below only when that fails.
                    pass
                else:
                    cmd = ["xdotool", "type", "--", text_to_type]
                    process = await subprocess.create_subprocess_exec(
                        *cmd,
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE
                    )
                    await self._communicate_or_kill(process, 0.5, "xdotool type co,end")
            except Exception as e: logger_webrtc_input.warning(f"Error with co,end type: {e}")
        elif msg_type == "_ebc":
            try:
                enable = toks[1].lower() == "true"
                self._spawn_task(self.update_binary_clipboard_setting(enable))
            except Exception as e:
                logger_webrtc_input.error(f"Error updating binary clipboard setting: {e}")
        elif msg_type == "_rc":
            try:
                mode = toks[1].strip().lower()
                rc_mode = RateControlMode(mode)
                self._spawn_task(self.on_update_rate_control_mode(rc_mode, display_id))
            except Exception as e:
                logger_webrtc_input.error(f"Error updating rate control mode: {e}")
        elif msg_type == "_crf":
            try:
                crf_value = int(toks[1])
                if not (0 <= crf_value <= 51):
                    logger_webrtc_input.warning(f"CRF value out of range (0-51): {crf_value}")
                    return
                self._spawn_task(self.on_update_crf(crf_value, display_id))
            except Exception as e:
                logger_webrtc_input.error(f"Error updating CRF value: {e}")
        elif toks[0].startswith("SETTINGS"):
            settings_data = ','.join(toks[1:]) if len(toks) > 1 else ""
            logger_webrtc_input.info(f"Received SETTINGS message: {settings_data}")
            try:
                settings_json = json.loads(settings_data)
                # Applied to the delivering channel's display (not a spoofable
                # payload field) and inline, so a resize behind it sees this policy.
                applied = self.on_update_settings(settings_json, display_id)
                if asyncio.iscoroutine(applied):
                    await applied
            except Exception as e:
                logger_webrtc_input.error(f"Failed to parse SETTINGS data: {e}")
        elif toks[0] == "SET_NATIVE_CURSOR_RENDERING":
            # WS-protocol alias of "p,N"; both map to the capture_cursor tunable.
            try:
                await self.on_mouse_pointer_visible(toks[1].strip().lower() in ("1", "true"))
            except (IndexError, ValueError) as e:
                logger_webrtc_input.warning(f"Malformed SET_NATIVE_CURSOR_RENDERING message: {msg[:60]}, error: {e}")
        elif toks[0] == "REQUEST_KEYFRAME":
            # Viewer-allowed IDR request, routed by delivering channel like RTCP PLI.
            _kf = self.on_request_keyframe(display_id)
            if asyncio.iscoroutine(_kf): await _kf
        else:
            logger_webrtc_input.info(f"Unknown data channel message: {msg[:100]}")

    def initialize_upload_dir(self) -> None:
        """Resolve and create the client-upload directory, refusing unsafe roots."""
        if self.upload_dir in ["/sys", "/proc", "/dev"]:
            logger_webrtc_input.info("Can not initialize upload directory at /sys /proc /dev locations")
            return
        if not self.upload_dir:
            logger_webrtc_input.info("Upload dir is empty")
            return

        if self.upload_dir == "~/Desktop":
            self.upload_dir_path = os.path.expanduser(self.upload_dir)
        else:
            self.upload_dir_path = self.upload_dir

        try:
            os.makedirs(self.upload_dir_path, exist_ok=True)
            logger_webrtc_input.info(f"Upload directory ensured: {self.upload_dir_path}")
        except OSError as e:
            logger_webrtc_input.error(f"Could not create upload directory {self.upload_dir_path}: {e}")
            self.upload_dir_path = None


MOUSE_POSITION = 10
MOUSE_MOVE = 11
MOUSE_SCROLL_UP = 20
MOUSE_SCROLL_DOWN = 21
MOUSE_SCROLL_LEFT = 22
MOUSE_SCROLL_RIGHT = 23
MOUSE_BUTTON_PRESS = 30
MOUSE_BUTTON_RELEASE = 31
MOUSE_BUTTON = 40
MOUSE_BUTTON_LEFT_ID = 41
MOUSE_BUTTON_MIDDLE_ID = 42
MOUSE_BUTTON_RIGHT_ID = 43

# Client button-mask bits translated before the per-bit diff: the pen eraser
# (Pointer Events button 5) drives the primary button (see send_x11_mouse).
MOUSE_MASK_BIT_PRIMARY = 1 << 0
MOUSE_MASK_BIT_ERASER = 1 << 5

# Codes for the uinput mouse helper socket.
UINPUT_BTN_LEFT = (EV_KEY, BTN_LEFT) 
UINPUT_BTN_MIDDLE = (EV_KEY, BTN_MIDDLE) 
UINPUT_BTN_RIGHT = (EV_KEY, BTN_RIGHT) 
# Relative axes: REL_X, REL_Y, REL_HWHEEL, REL_WHEEL.
UINPUT_REL_X = (EV_REL, 0x00)
UINPUT_REL_Y = (EV_REL, 0x01)
UINPUT_REL_HWHEEL = (EV_REL, 0x06)
UINPUT_REL_WHEEL = (EV_REL, 0x08)

# X core pointer button numbers for XTEST fake_input.
XBUTTON_LEFT = 1
XBUTTON_MIDDLE = 2
XBUTTON_RIGHT = 3

MOUSE_BUTTON_MAP = {
    MOUSE_BUTTON_LEFT_ID: {"uinput": UINPUT_BTN_LEFT, "x11": XBUTTON_LEFT},
    MOUSE_BUTTON_MIDDLE_ID: {"uinput": UINPUT_BTN_MIDDLE, "x11": XBUTTON_MIDDLE},
    MOUSE_BUTTON_RIGHT_ID: {"uinput": UINPUT_BTN_RIGHT, "x11": XBUTTON_RIGHT},
}
