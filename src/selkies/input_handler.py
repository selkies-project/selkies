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

import ctypes
import logging
import select
import shutil
import struct
import threading
import time
import asyncio
from asyncio import subprocess
import socket
import os
import base64
import binascii
import io
import re
import json
import aiofiles
import msgpack
from PIL import Image
import urllib.parse
import urllib.request
from .media_pipeline import RateControlMode
from .settings import settings, WS_MAX_MESSAGE_BYTES
try:
    libxkb = ctypes.CDLL("libxkbcommon.so.0")
    libxkb.xkb_keysym_to_utf8.argtypes = [ctypes.c_uint32, ctypes.c_char_p, ctypes.c_size_t]
    libxkb.xkb_keysym_to_utf8.restype = ctypes.c_int
    # Keymap-compilation surface for _WaylandKeymapOwner: compile a keymap string
    # and enumerate keysyms per keycode/level.
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
except Exception:
    libxkb = None

try:
    from . import Xlib
    from .Xlib import display
    from .Xlib import X
    from .Xlib import XK
    from .Xlib.ext import xfixes, xtest
    from .Xlib.protocol import event as xevent
    X11_LIBS_AVAILABLE = True
except ImportError:
    X11_LIBS_AVAILABLE = False
    Xlib = None
    display = None
    X = None
    XK = None
    xfixes = None
    xtest = None
    xevent = None

try:
    from pixelflux import ScreenCapture
except (ImportError, RuntimeError):
    ScreenCapture = None

# Server-side input authority, shared by both transports so a modified client
# can't inject input a controller didn't grant. A read-only viewer peer
# (shared/#player co-op) may only send VIEWER_ALLOWED_PREFIXES; a viewer
# holding the active mk token while enable_collab is on (a read-write
# collaborator) may additionally send the keyboard/mouse/clipboard set below.
# cmd and other settings-mutating messages stay controller-only on both.
VIEWER_ALLOWED_PREFIXES = (
    "SETTINGS,",
    "START_VIDEO",
    # A viewer must be able to pause its own video feed on tab hide (the client
    # sends this); without it a broadcast-pause can never trigger for viewers.
    "STOP_VIDEO",
    "REQUEST_KEYFRAME",
    "js,",
)
VIEWER_COLLAB_EXTRA_PREFIXES = (
    "kd", "ku", "kh", "kr", "m", "m2",
    "cws", "cbs", "cwd", "cbd", "cwe", "cbe", "cw", "cb", "cr",
    "REQUEST_CLIPBOARD",
)
# Lifecycle noise every client emits on blur/visibility changes (kr =
# release-all, cr = clipboard read-back): a read-only viewer sending them is
# normal operation, so they are dropped without a warning.
VIEWER_SILENT_DROP_PREFIXES = ("kr", "cr")


class _WaylandKeymapOwner:
    """Owns keysym policy for the Wayland compositor: resolves keysyms to keycodes
    against the compositor's own keymap (synthesizing Shift/AltGr for leveled
    glyphs) and binds unmapped keysyms — Unicode codepoints, IME output — to spare
    overlay keycodes by swapping in a rebuilt keymap. Everything is delivered
    through inject_key / set_keymap_string on the one compositor channel, so key
    ordering holds. Raises on failure so the caller can fall back."""

    _OVERLAY_SLOTS = 16
    # Past the evdev/pc105 base range; pure-Wayland clients look keycodes up in
    # the delivered keymap, so exceeding X11's 255 ceiling is safe here.
    _OVERLAY_BASE_KEYCODE = 257

    def __init__(self, wayland_input, base_keymap_text):
        if libxkb is None:
            raise RuntimeError("libxkbcommon unavailable")
        if not base_keymap_text:
            raise RuntimeError("empty compositor keymap")
        self._input = wayland_input
        self._base_text = base_keymap_text
        self._map = {}            # keysym -> (keycode, level)
        self._overlay = {}        # keysym -> overlay keycode
        self._overlay_order = []  # round-robin recycle order
        self._pressed = {}        # keysym -> (keycode, modifier keycodes)
        self._mod_refs = {}       # modifier keycode -> holders
        self._build_map()

    def _build_map(self):
        ctx = libxkb.xkb_context_new(0)
        if not ctx:
            raise RuntimeError("xkb_context_new failed")
        try:
            km = libxkb.xkb_keymap_new_from_string(
                ctx, self._base_text.encode(), 1, 0)  # TEXT_V1, NO_FLAGS
            if not km:
                raise RuntimeError("keymap compile failed")
            try:
                lo = libxkb.xkb_keymap_min_keycode(km)
                hi = libxkb.xkb_keymap_max_keycode(km)
                syms = ctypes.POINTER(ctypes.c_uint32)()
                for kc in range(lo, hi + 1):
                    levels = min(4, libxkb.xkb_keymap_num_levels_for_key(km, kc, 0))
                    for level in range(levels):
                        n = libxkb.xkb_keymap_key_get_syms_by_level(
                            km, kc, 0, level, ctypes.byref(syms))
                        for i in range(n):
                            sym = syms[i]
                            if sym and sym not in self._map:
                                self._map[sym] = (kc, level)
            finally:
                libxkb.xkb_keymap_unref(km)
        finally:
            libxkb.xkb_context_unref(ctx)
        # Modifier keycodes for level synthesis; conventional evdev+8 fallbacks.
        self._shift_kc = self._map.get(0xFFE1, (50, 0))[0]
        self._altgr_kc = self._map.get(0xFE03, (108, 0))[0]

    def _mods_for_level(self, level):
        mods = []
        if level & 1:
            mods.append(self._shift_kc)
        if level & 2:
            mods.append(self._altgr_kc)
        return tuple(mods)

    def _overlay_text(self):
        """Base keymap text with the overlay keycodes appended and every occupied
        slot bound to its keysym at level 0 (hex keysym literals need no names)."""
        base = self._base_text
        max_at = base.index("maximum = ")
        max_end = base.index(";", max_at)
        parts = [base[:max_at],
                 f"maximum = {self._OVERLAY_BASE_KEYCODE + self._OVERLAY_SLOTS}"]
        rest = base[max_end:]
        kc_end = rest.index("};")
        parts.append(rest[:kc_end])
        for i in range(self._OVERLAY_SLOTS):
            parts.append(f"\t<UC{i + 1:02}> = {self._OVERLAY_BASE_KEYCODE + i};\n")
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
            slot = kc - self._OVERLAY_BASE_KEYCODE
            parts.append(f"\tkey <UC{slot + 1:02}> {{ [ {keysym:#x} ] }};\n")
        parts.append(rest[close_at:])
        return "".join(parts)

    def _overlay_bind(self, keysym):
        kc = self._overlay.get(keysym)
        if kc is not None:
            return kc
        if len(self._overlay) >= self._OVERLAY_SLOTS:
            oldest = self._overlay_order.pop(0)
            kc = self._overlay.pop(oldest)
        else:
            kc = self._OVERLAY_BASE_KEYCODE + len(self._overlay)
        self._overlay[keysym] = kc
        self._overlay_order.append(keysym)
        # Clients receive the new keymap before the key event that uses it
        # (same wl_keyboard event stream), so pressing immediately is safe.
        self._input.set_keymap_string(self._overlay_text())
        return kc

    def press(self, keysym):
        held = self._pressed.get(keysym)
        if held is not None:
            # Auto-repeat re-press: re-inject the key only; modifier refcounts and
            # the held map were charged by the first press.
            self._input.inject_key(held[0], 1)
            return
        resolved = self._map.get(keysym)
        if resolved is not None:
            kc, level = resolved
            mods = self._mods_for_level(level)
        else:
            kc = self._overlay_bind(keysym)
            mods = ()
        for m in mods:
            self._mod_refs[m] = self._mod_refs.get(m, 0) + 1
            if self._mod_refs[m] == 1:
                self._input.inject_key(m, 1)
        self._pressed[keysym] = (kc, mods)
        self._input.inject_key(kc, 1)

    def release(self, keysym):
        held = self._pressed.pop(keysym, None)
        if held is None:
            return
        kc, mods = held
        self._input.inject_key(kc, 0)
        for m in reversed(mods):
            refs = self._mod_refs.get(m, 0) - 1
            if refs <= 0:
                self._mod_refs.pop(m, None)
                self._input.inject_key(m, 0)
            else:
                self._mod_refs[m] = refs

    def reset(self):
        """Release every held key and synthetic modifier."""
        for keysym in list(self._pressed):
            try:
                self.release(keysym)
            except Exception:
                self._pressed.pop(keysym, None)
        for m in list(self._mod_refs):
            try:
                self._input.inject_key(m, 0)
            except Exception:
                pass
        self._mod_refs.clear()


class _X11ClipboardMonitor:
    """Event-driven X11 CLIPBOARD access on a dedicated Display connection:
    XFixes selection-owner events signal changes (no polling, no xclip forks) and
    reads go through ConvertSelection with INCR support for large payloads.
    Every Display call runs on this class's own event thread (python-xlib is not
    thread-safe); callers hand it work via a self-pipe. Writes are native too:
    offer() takes CLIPBOARD ownership and the event thread serves SelectionRequest
    (TARGETS + text aliases, or the stored image mime) until another app takes over
    (SelectionClear). xclip remains only as the caller's fallback."""

    _READ_TIMEOUT_S = 5.0
    # INCR reads accumulate at most this much (matches the Wayland read cap).
    _READ_MAX_BYTES = 64 * 1024 * 1024
    # Per-ChangeProperty chunk: stays under the classic 256 KiB X11 request limit
    # (python-xlib has no BIG-REQUESTS); large payloads append in chunks BEFORE the
    # SelectionNotify, so requestors read one complete property and INCR is not needed.
    _WRITE_CHUNK = 240 * 1024

    def __init__(self, display_name=None):
        self._d = display.Display(display_name)
        if not self._d.has_extension('XFIXES'):
            self._d.close()
            raise RuntimeError("XFixes not available")
        self._d.xfixes_query_version()
        screen = self._d.screen()
        # Unmapped InputOutput window: receives SelectionNotify/PropertyNotify and
        # holds the transfer property; never mapped, so never visible.
        self._win = screen.root.create_window(
            0, 0, 1, 1, 0, screen.root_depth, window_class=X.InputOutput,
            event_mask=X.PropertyChangeMask)
        self._clipboard = self._d.get_atom('CLIPBOARD')
        self._prop = self._d.get_atom('SELKIES_CLIP')
        self._incr = self._d.get_atom('INCR')
        self._targets = self._d.get_atom('TARGETS')
        # Pre-intern every target we can consume (all Display calls happen here,
        # before the event thread starts; read() then only compares atom ids).
        self._image_targets = [(self._d.get_atom(m), m) for m in (
            'image/png', 'image/jpeg', 'image/bmp', 'image/webp', 'image/svg+xml')]
        self._text_targets = [(self._d.get_atom(t), t) for t in (
            'UTF8_STRING', 'text/plain;charset=utf-8', 'STRING')]
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
        self._read_lock = threading.Lock()  # one in-flight conversion at a time
        # Owned-selection payload, staged by offer() and served by the event thread.
        self._own_data = None
        self._own_mime_atom = None
        self._own_is_text = False
        self._pending_own = None
        self._own_done = threading.Event()
        self._own_ok = False
        self._cmd_r, self._cmd_w = os.pipe()
        self._stop = False
        self._thread = threading.Thread(target=self._event_loop, daemon=True,
                                        name="X11ClipboardMonitor")
        self._thread.start()

    # ---- event thread ----

    def _event_loop(self):
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

    def _dispatch_event(self, ev):
        if isinstance(ev, xfixes.SelectionNotify):
            self._changed.set()
        elif ev.type == X.SelectionNotify:
            self._collect_selection(ev)
        elif ev.type == X.SelectionRequest:
            self._serve_selection(ev)
        elif ev.type == X.SelectionClear:
            # Another app owns the clipboard now; keep the payload only for
            # requests already racing in — new content comes via the monitor.
            self._own_data = None
            self._own_mime_atom = None

    def _take_ownership(self, payload):
        data, mime_atom, is_text = payload
        try:
            self._own_data = data
            self._own_mime_atom = mime_atom
            self._own_is_text = is_text
            self._win.set_selection_owner(self._clipboard, X.CurrentTime)
            self._d.flush()
            owner = self._d.get_selection_owner(self._clipboard)
            self._own_ok = (getattr(owner, 'id', owner) == self._win.id)
        except Exception:
            self._own_ok = False
        self._own_done.set()

    def _serve_selection(self, ev):
        """Answer a SelectionRequest for the payload offer() staged (ICCCM)."""
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
                # Chunked appends stay under the 256 KiB request limit; the property
                # is complete before the notify below, so no INCR is needed.
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

    def _prop_bytes(self, prop):
        v = prop.value
        if isinstance(v, str):
            return v.encode('latin-1')
        if isinstance(v, (bytes, bytearray)):
            return bytes(v)
        return bytes(bytearray(v))

    def _collect_selection(self, ev):
        """On the event thread: fetch the converted property (INCR-aware)."""
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
                # INCR: each property delete requests the next chunk; a zero-length
                # chunk ends the transfer. Wait for events with the remaining
                # deadline — a stalled owner must time the read out, not wedge the
                # event thread inside a blocking next_event(). Total size is capped
                # like the Wayland read so a hostile owner cannot balloon memory.
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
                        # Keep serving paste requests mid-INCR read.
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

    # ---- caller side (blocking; run via executor) ----

    def _convert_and_wait(self, target_atom):
        with self._read_lock:
            self._reply = None
            self._reply_done.clear()
            self._pending_target = target_atom
            os.write(self._cmd_w, b"x")
            if not self._reply_done.wait(self._READ_TIMEOUT_S):
                return None
            return self._reply

    def read(self, use_binary):
        """Blocking read (call via executor): (data, mime) like read_clipboard —
        text as str with mime 'text/plain', images as bytes with their mime."""
        reply = self._convert_and_wait(self._targets)
        if not reply or reply[1] != 32:
            # A fresh owner (e.g. xclip mid-fork) may not serve requests for a
            # moment after the owner-change event; one short retry covers it.
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
        for atom, _name in self._text_targets:
            if atom in offered:
                got = self._convert_and_wait(atom)
                if got is not None and got[0] is not None:
                    return bytes(got[0]).decode('utf-8', errors='replace'), 'text/plain'
        return None, None

    def offer(self, data, mime_type):
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

    async def wait_change(self, timeout):
        """Await a selection-owner change (True) or timeout (False)."""
        loop = asyncio.get_running_loop()
        got = await loop.run_in_executor(None, self._changed.wait, timeout)
        if got:
            self._changed.clear()
        return got

    def close(self):
        self._stop = True
        try:
            os.write(self._cmd_w, b"q")
        except OSError:
            pass
        # Let the event thread leave its select() before the display closes under it.
        self._thread.join(timeout=2.0)
        try:
            self._d.close()
        except Exception:
            pass


class _XTestKeyboard:
    """Keyboard controller backed by the bundled python-xlib XTEST extension.
    Injects key events through the already-open self.xdisplay connection; a
    separate second X-display connection whose blocking sync could spin at
    100% CPU inside connect() is deliberately avoided."""

    # Spare keycodes past the base layout, bound on demand to keysyms the layout
    # lacks (Unicode, exotic symbols) so they inject in-process via XTEST instead of
    # forking xdotool. Round-robin recycled; the reverse of TigerVNC/x0vncserver's
    # XkbAddKeyKeysym, done through core ChangeKeyboardMapping.
    _OVERLAY_SLOTS = 16

    def __init__(self, xdisplay):
        self._d = xdisplay
        # XK_Shift_L; may be 0 on an exotic keymap (then capitals just skip shift).
        self._shift_kc = xdisplay.keysym_to_keycode(0xffe1)
        # XK_Shift_R: a client-held Shift may be down on either keycode.
        self._shift_r_kc = xdisplay.keysym_to_keycode(0xffe2)
        # AltGr, to reach glyphs bound above the Shift level (e.g. '@' on an Italian
        # or German keymap): prefer ISO_Level3_Shift, fall back to Mode_switch.
        self._altgr_kc = (xdisplay.keysym_to_keycode(0xfe03)
                          or xdisplay.keysym_to_keycode(0xff7e))
        # keysym -> the modifier keycodes press() synthesized for it. release() undoes
        # only these, so a modifier the client is physically holding (injected via the
        # XTEST fast path) is never force-released here.
        self._synth_mods = {}
        # Dynamic-overlay state (lazy): spare keycodes, keysym->keycode bindings,
        # and the keycode used at PRESS so release replays it (never re-resolves,
        # matching neko's XKeyEntryGet — the layout may shift mid-keystroke).
        self._spare_keycodes = None
        self._overlay = {}          # keysym -> keycode
        self._overlay_order = []    # round-robin recycle order
        self._pressed_kc = {}       # keysym -> keycode injected at press

    def _find_spare_keycodes(self):
        """Keycodes whose every level is NoSymbol — free to repurpose."""
        info = self._d.display.info
        lo, hi = info.min_keycode, info.max_keycode
        mapping = self._d.get_keyboard_mapping(lo, hi - lo + 1)
        spares = []
        for i, syms in enumerate(mapping):
            if all(s == 0 for s in syms):
                spares.append(lo + i)
            if len(spares) >= self._OVERLAY_SLOTS:
                break
        return spares

    def _overlay_keycode(self, keysym):
        """Bind an unmapped keysym to a spare keycode (recycling the oldest) and
        return it, or None if no spare keycode exists."""
        if keysym in self._overlay:
            return self._overlay[keysym]
        if self._spare_keycodes is None:
            self._spare_keycodes = self._find_spare_keycodes()
        if not self._spare_keycodes:
            return None
        if len(self._overlay) >= len(self._spare_keycodes):
            oldest = self._overlay_order.pop(0)
            kc = self._overlay.pop(oldest)
        else:
            kc = self._spare_keycodes[len(self._overlay)]
        # Assign the keysym at levels 0 and 1 so an accidental Shift can't change it.
        self._d.change_keyboard_mapping(kc, [[keysym, keysym]])
        self._d.sync()
        self._overlay[keysym] = kc
        self._overlay_order.append(keysym)
        return kc

    def _resolve(self, keysym):
        """Return (keycode, modifier_keycodes) to inject this keysym. The modifiers are
        the Shift / AltGr keycodes whose held state selects the keymap level the keysym
        sits at, so a glyph bound above the Shift level (e.g. AltGr '@') types correctly
        instead of falling through to its level-0 glyph."""
        d = self._d
        kc = d.keysym_to_keycode(keysym)
        if not kc:
            # Not in the layout: map it to a spare keycode in-process (no xdotool
            # fork). Overlay keysyms sit at level 0, so they never need modifiers.
            kc = self._overlay_keycode(keysym)
            if not kc:
                # No spare keycode: raise so the caller falls back to xdotool.
                raise ValueError("no keycode for keysym %r" % (keysym,))
            return kc, ()
        # Lowest column carrying this glyph: 0 base, 1 Shift, 2 AltGr, 3 Shift+AltGr.
        level = next((lvl for lvl in range(4)
                      if d.keycode_to_keysym(kc, lvl) == keysym), 0)
        mods = []
        if level & 1 and self._shift_kc:
            mods.append(self._shift_kc)
        if level & 2 and self._altgr_kc:
            mods.append(self._altgr_kc)
        return kc, tuple(mods)

    def _shift_down(self):
        # Logical Shift state: one round trip on the already-open display,
        # queried only for shifted keysyms.
        bits = self._d.query_keymap()
        return any(kc and bits[kc // 8] & (1 << (kc % 8))
                   for kc in (self._shift_kc, self._shift_r_kc))

    def _mod_down(self, kc):
        # Is this modifier keycode already held (by the client, via the fast path)?
        # Shift may be held on either Shift_L or Shift_R.
        if not kc:
            return False
        if kc == self._shift_kc:
            return self._shift_down()
        bits = self._d.query_keymap()
        return bool(bits[kc // 8] & (1 << (kc % 8)))

    def press(self, keysym):
        kc, mods = self._resolve(keysym)
        # Synthesize only modifiers not already held; release only those on release().
        synth = [m for m in mods if not self._mod_down(m)]
        for m in synth:
            xtest.fake_input(self._d, Xlib.X.KeyPress, m)
        if synth:
            self._synth_mods[keysym] = synth
        xtest.fake_input(self._d, Xlib.X.KeyPress, kc)
        self._pressed_kc[keysym] = kc  # replay this exact keycode on release
        self._d.flush()

    def release(self, keysym):
        # Replay the press-time keycode; only re-resolve if the press wasn't tracked
        # (the layout may have changed mid-keystroke, orphaning a re-resolve).
        kc = self._pressed_kc.pop(keysym, None)
        if kc is None:
            kc, _ = self._resolve(keysym)
        xtest.fake_input(self._d, Xlib.X.KeyRelease, kc)
        for m in reversed(self._synth_mods.pop(keysym, ())):
            xtest.fake_input(self._d, Xlib.X.KeyRelease, m)
        self._d.flush()


class _XTestMouse:
    """Mouse controller backed by the bundled python-xlib XTEST extension."""

    def __init__(self, xdisplay):
        self._d = xdisplay

    @property
    def position(self):
        p = self._d.screen().root.query_pointer()
        return (p.root_x, p.root_y)

    @position.setter
    def position(self, xy):
        x, y = xy
        xtest.fake_input(self._d, Xlib.X.MotionNotify, detail=False,
                         root=Xlib.X.NONE, x=int(x), y=int(y))
        self._d.flush()

    def scroll(self, dx, dy):
        d = self._d
        # X core pointer scroll buttons: 4=up, 5=down, 6=left, 7=right. Sign
        # convention matches the callers (positive dy = up, positive dx = right),
        # so the end-to-end wheel direction is unchanged from the prior Controller.
        def _clicks(btn, n):
            for _ in range(int(abs(n))):
                xtest.fake_input(d, Xlib.X.ButtonPress, btn)
                xtest.fake_input(d, Xlib.X.ButtonRelease, btn)
        if dy:
            _clicks(4 if dy > 0 else 5, dy)
        if dx:
            _clicks(7 if dx > 0 else 6, dx)
        d.flush()

    def press(self, button):
        xtest.fake_input(self._d, Xlib.X.ButtonPress, int(button))
        self._d.flush()

    def release(self, button):
        xtest.fake_input(self._d, Xlib.X.ButtonRelease, int(button))
        self._d.flush()


logger_webrtc_input = logging.getLogger("webrtc_input")
logger_selkies_gamepad = logging.getLogger("selkies_gamepad")

# Upper bound on a single multi-part clipboard transfer (bytes). The declared
# size and the accumulated chunks are both checked against this so a client
# cannot grow the in-memory buffer without bound (memory-exhaustion DoS).
MULTIPART_CLIPBOARD_MAX_SIZE = 64 * 1024 * 1024


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
        # Raised for paths on different drives or a mix of absolute/relative.
        return False

# EVDEV Event Codes (from linux/input-event-codes.h)
EV_SYN = 0x00
EV_KEY = 0x01
EV_REL = 0x02
EV_ABS = 0x03
EV_MSC = 0x04
SYN_REPORT = 0

# Mouse Button Codes (from linux/input-event-codes.h)
BTN_MOUSE = 0x110
BTN_LEFT = 0x110
BTN_RIGHT = 0x111
BTN_MIDDLE = 0x112
BTN_SIDE = 0x113
BTN_EXTRA = 0x114

# Gamepad Button Codes
BTN_A = 0x130       # Or BTN_SOUTH
BTN_B = 0x131       # Or BTN_EAST
BTN_C = 0x132       # Typically BTN_C in evdev, for matching XBox360 bitmask
BTN_X = 0x133       # Or BTN_NORTH
BTN_Y = 0x134       # Or BTN_WEST
BTN_Z = 0x135       # Typically BTN_Z in evdev, for matching XBox360 bitmask
BTN_TL = 0x136      # Left Bumper
BTN_TR = 0x137      # Right Bumper
BTN_SELECT = 0x13a  # Back button
BTN_START = 0x13b   # Start button
BTN_MODE = 0x13c    # Xbox/Guide button
BTN_THUMBL = 0x13d  # Left Thumbstick click
BTN_THUMBR = 0x13e  # Right Thumbstick click


# Absolute Axis Codes
ABS_X = 0x00
ABS_Y = 0x01
ABS_Z = 0x02      # Often Left Trigger
ABS_RX = 0x03
ABS_RY = 0x04
ABS_RZ = 0x05     # Often Right Trigger
ABS_HAT0X = 0x10
ABS_HAT0Y = 0x11

# JS Event types (from linux/joystick.h, used by the JS-like interface)
JS_EVENT_BUTTON = 0x01
JS_EVENT_AXIS = 0x02
JS_EVENT_INIT = 0x80

# For js_config_t struct packing for the C interposer
# These are the max sizes in the C struct js_config_t
INTERPOSER_MAX_BTNS = 512
INTERPOSER_MAX_AXES = 64
CONTROLLER_NAME_MAX_LEN = 255 
C_INTERPOSER_STRUCT_SIZE = 1360

# Raw bytes per multipart message: fill the shared WebSocket frame ceiling, with
# margin for the verb prefix and base64 expansion (limit/4*3 of the payload budget).
CLIPBOARD_CHUNK_SIZE = ((WS_MAX_MESSAGE_BYTES - 4096) * 3) // 4 

# For mouse input to send fake back and forward events
KEYSYM_ALT_L = 0xFFE9     # Left Alt keysym
KEYSYM_LEFT_ARROW = 0xFF51 # Left Arrow keysym
KEYSYM_RIGHT_ARROW = 0xFF53# Right Arrow keysym

# Import keysyms
try:
    from .server_keysym_map import X11_KEYSYM_MAP
except ImportError:
    logger_webrtc_input = logging.getLogger("webrtc_input_fallback_map_import")
    logger_webrtc_input.warning(
        "server_keysym_map.py not found or X11_KEYSYM_MAP not defined. "
        "Keysym mapping will rely entirely on fallback."
    )
    X11_KEYSYM_MAP = {}

CYRILLIC_TO_QWERTY_KEYSYM = {
    0x06CA: 0x0071, # Й -> q
    0x06C3: 0x0077, # Ц -> w
    0x06D5: 0x0065, # У -> e
    0x06CB: 0x0072, # К -> r
    0x06C5: 0x0074, # Е -> t
    0x06CE: 0x0079, # Н -> y
    0x06C7: 0x0075, # Г -> u
    0x06DB: 0x0069, # Ш -> i
    0x06DD: 0x006F, # Щ -> o
    0x06DA: 0x0070, # З -> p
    0x06C6: 0x0061, # Ф -> a
    0x06D9: 0x0073, # Ы -> s
    0x06D7: 0x0064, # В -> d
    0x06C1: 0x0066, # А -> f
    0x06D0: 0x0067, # П -> g
    0x06D2: 0x0068, # Р -> h
    0x06CF: 0x006A, # О -> j
    0x06CC: 0x006B, # Л -> k
    0x06C4: 0x006C, # Д -> l
    0x06D1: 0x007A, # Я -> z
    0x06DE: 0x0078, # Ч -> x
    0x06D3: 0x0063, # С -> c
    0x06CD: 0x0076, # М -> v
    0x06C9: 0x0062, # И -> b
    0x06D4: 0x006E, # Т -> n
    0x06D8: 0x006D, # Ь -> m
}

class JsConfigCtypes(ctypes.Structure):
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

# Get the size of the C-compatible struct
EXPECTED_C_STRUCT_SIZE = ctypes.sizeof(JsConfigCtypes)
logging.info(f"Expected C js_config_t size (from ctypes): {EXPECTED_C_STRUCT_SIZE} bytes")


ABS_MIN_VAL = -32767
ABS_MAX_VAL = 32767
ABS_TRIGGER_MIN_VAL = 0 # Triggers often 0-255 or 0-1023 for EVDEV
ABS_TRIGGER_MAX_VAL = 255 # Or 1023, or ABS_MAX_VAL depending on driver expectation
ABS_HAT_MIN_VAL = -1
ABS_HAT_MAX_VAL = 1

STANDARD_XPAD_CONFIG = {
    "name": "Microsoft X-Box 360 pad",
    "vendor_id": 0x045e,
    "product_id": 0x028e,
    "version": 0x0114,

    # EVDEV codes. The order here defines our internal abstract button indices.
    "btn_map": [
        BTN_A,      # Internal abstract button 0
        BTN_B,      # Internal abstract button 1
        BTN_X,      # Internal abstract button 2
        BTN_Y,      # Internal abstract button 3
        BTN_TL,     # Internal abstract button 4 (Left Bumper)
        BTN_TR,     # Internal abstract button 5 (Right Bumper)
        BTN_SELECT, # Internal abstract button 6 (Back)
        BTN_START,  # Internal abstract button 7 (Start)
        BTN_MODE,   # Internal abstract button 8 (Xbox Guide)
        BTN_THUMBL, # Internal abstract button 9 (Left Stick Click)
        BTN_THUMBR, # Internal abstract button 10 (Right Stick Click)
    ],

    # EVDEV codes for axes. The order defines internal abstract axis indices.
    "axes_map": [
        ABS_X,     # Internal abstract axis 0 (Left Stick X)
        ABS_Y,     # Internal abstract axis 1 (Left Stick Y)
        ABS_Z,     # Internal abstract axis 2 (Left Trigger)
        ABS_RX,    # Internal abstract axis 3 (Right Stick X)
        ABS_RY,    # Internal abstract axis 4 (Right Stick Y)
        ABS_RZ,    # Internal abstract axis 5 (Right Trigger)
        ABS_HAT0X, # Internal abstract axis 6 (D-Pad X)
        ABS_HAT0Y  # Internal abstract axis 7 (D-Pad Y)
    ],

    "mapping": {
        # Maps client button numbers to our internal abstract button *indices*.
        "btns": { # client_btn_idx -> internal_abstract_btn_idx
            0: 0,  # Client A -> internal index 0 (BTN_A)
            1: 1,  # Client B -> internal index 1 (BTN_B)
            2: 2,  # Client X -> internal index 2 (BTN_X)
            3: 3,  # Client Y -> internal index 3 (BTN_Y)
            4: 4,  # Client LB -> internal index 4 (BTN_TL)
            5: 5,  # Client RB -> internal index 5 (BTN_TR)
            8: 6,  # Client Select/Back -> internal index 6 (BTN_SELECT)
            9: 7,  # Client Start -> internal index 7 (BTN_START)
            10: 9, # Client Left Stick Press -> internal index 9 (BTN_THUMBL)
            11: 10,# Client Right Stick Press -> internal index 10 (BTN_THUMBR)
            16: 8, # Client Xbox/Home -> internal index 8 (BTN_MODE)
        },
        "axes": { # client_axis_idx -> internal_abstract_axis_idx
            0: 0, # Client Left Stick X  -> internal index 0 (ABS_X)
            1: 1, # Client Left Stick Y  -> internal index 1 (ABS_Y)
            2: 3, # Client Right Stick X -> internal index 3 (ABS_RX)
            3: 4, # Client Right Stick Y -> internal index 4 (ABS_RY)
        },
        # Client buttons that map to an internal abstract axis
        "client_btns_to_internal_axes": {
            6: 2, # Client Btn 6 (LT) -> internal axis 2 (ABS_Z)
            7: 5, # Client Btn 7 (RT) -> internal axis 5 (ABS_RZ)
        },
        # Client DPad buttons map to internal abstract HAT axes
        "dpad_to_hat": {
            # client_btn_idx -> (internal_abstract_axis_idx_for_HAT, hat_direction_value)
            12: (7, -1), # Up    -> internal axis 7 (ABS_HAT0Y), value -1
            13: (7, 1),  # Down  -> internal axis 7 (ABS_HAT0Y), value 1
            14: (6, -1), # Left  -> internal axis 6 (ABS_HAT0X), value -1
            15: (6, 1),  # Right -> internal axis 6 (ABS_HAT0X), value 1
        },
        "trigger_internal_abstract_axis_indices": [2, 5],
        "hat_internal_abstract_axis_indices": [6, 7],
    }
}

# --- Event Packing Functions ---
def get_js_event_packed(ev_type, number, value):
    """Packs a js_event struct."""
    # struct js_event { __u32 time; __s16 value; __u8 type; __u8 number; };
    # `type` is __u8, so pack it as 'B' — 'b' (signed) raises struct.error the
    # moment the 0x80 JS_EVENT_INIT flag is OR'd into the type (value >= 128).
    ts_ms = int(time.time() * 1000) & 0xFFFFFFFF # Ensure it fits in u32
    return struct.pack("=IhBB", ts_ms, int(value), ev_type, number)

def get_evdev_events_packed(ev_type, ev_code, ev_value, client_arch_bits):
    """Packs an input_event struct and a SYN_REPORT, using client architecture for timeval."""
    # struct input_event { struct timeval time; __u16 type; __u16 code; __s32 value; };
    # struct timeval { time_t tv_sec; suseconds_t tv_usec; };
    # time_t and suseconds_t are 'long' on 32-bit, 'long long' (usually) on 64-bit for tv_sec,
    # and 'long' for tv_usec. The C interposer sends sizeof(unsigned long).
    
    now = time.time()
    ts_sec = int(now)
    ts_usec = int((now - ts_sec) * 1_000_000)

    if client_arch_bits == 64: # Assuming 'long' is 8 bytes for timeval members on 64-bit client
        timeval_fmt = "qq" # tv_sec (long long), tv_usec (long long)
    else: # Assuming 'long' is 4 bytes for timeval members on 32-bit client
        timeval_fmt = "ll" # tv_sec (long), tv_usec (long)
    
    event_fmt = f"={timeval_fmt}HHi" # Native byte order, timeval, type, code, value

    event_data = struct.pack(event_fmt, ts_sec, ts_usec, ev_type, ev_code, int(ev_value))
    syn_event_data = struct.pack(event_fmt, ts_sec, ts_usec, EV_SYN, SYN_REPORT, 0)
    return event_data + syn_event_data

def normalize_axis_value(client_value, is_trigger, is_hat, for_js_event=False):
    """
    Normalizes client axis value.
    If for_js_event is True and is_hat is True, it scales to the full axis range.
    """
    if is_hat:
        hat_val = int(max(ABS_HAT_MIN_VAL, min(ABS_HAT_MAX_VAL, round(client_value))))
        if for_js_event:
            # For JS, D-pad axes need to be full range, not -1/0/1
            return hat_val * ABS_MAX_VAL
        else:
            # For EVDEV, HAT values are -1, 0, or 1
            return hat_val
    if is_trigger: # Client sends 0.0 to 1.0
        # For JS and EVDEV, triggers are often treated as regular axes.
        # Map 0..1 to -32k..+32k for consistency, some drivers map to 0..255.
        # This mapping ensures it works like an analog input.
        return int(ABS_MIN_VAL + client_value * (ABS_MAX_VAL - ABS_MIN_VAL))
    # Regular axis: client sends -1.0 to 1.0
    return int(ABS_MIN_VAL + ((client_value + 1) / 2) * (ABS_MAX_VAL - ABS_MIN_VAL))


class GamepadMapper:
    def __init__(self, config_template, client_input_name, client_num_btns, client_num_axes):
        self.config = config_template
        self.client_input_name = client_input_name

    def get_mapped_events(self, client_event_idx, client_value, is_button_event):
        internal_abstract_idx = -1
        is_trigger_axis = False
        is_hat_axis = False
        target_evdev_type = None
        final_value = 0 # This will be the raw value from the client or dpad direction

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
        else: # Axis event
            internal_abstract_idx = self.config["mapping"]["axes"].get(client_event_idx)
            is_trigger_axis = internal_abstract_idx in self.config["mapping"]["trigger_internal_abstract_axis_indices"]
            is_hat_axis = internal_abstract_idx in self.config["mapping"]["hat_internal_abstract_axis_indices"]
            target_evdev_type = EV_ABS
            final_value = client_value

        if internal_abstract_idx is None or internal_abstract_idx < 0:
            return None

        # 2. Get EVDEV code and normalized values for both JS and EVDEV
        evdev_code = -1
        js_event_value = 0
        evdev_event_value = 0

        if target_evdev_type == EV_KEY:
            if 0 <= internal_abstract_idx < len(self.config["btn_map"]):
                evdev_code = self.config["btn_map"][internal_abstract_idx]
                js_event_value = evdev_event_value = final_value # 0 or 1
            else: return None
        elif target_evdev_type == EV_ABS:
            if 0 <= internal_abstract_idx < len(self.config["axes_map"]):
                evdev_code = self.config["axes_map"][internal_abstract_idx]
                # Calculate values separately for JS and EVDEV
                js_event_value = normalize_axis_value(final_value, is_trigger_axis, is_hat_axis, for_js_event=True)
                evdev_event_value = normalize_axis_value(final_value, is_trigger_axis, is_hat_axis, for_js_event=False)
            else: return None
        else:
            return None

        # 3. Create event data/templates
        if evdev_code != -1:
            js_event_type = JS_EVENT_BUTTON if target_evdev_type == EV_KEY else JS_EVENT_AXIS
            js_event_data = get_js_event_packed(js_event_type, internal_abstract_idx, js_event_value)
            
            evdev_event_template = (target_evdev_type, evdev_code, evdev_event_value)
            
            return {'js_event_data': js_event_data, 'evdev_event_template': evdev_event_template}
        
        return None

# Process-wide virtual gamepad instances, keyed by slot index. Apps open the
# interposer sockets ONCE at their own startup (the .so presents them as
# /dev/input devices), so the instances — and the bound sockets — must outlive
# every per-service input handler: a transport mode switch (websockets <->
# webrtc) tears one service down and starts the other, and closing/rebinding
# the sockets there would leave every running app holding a dead fd until the
# app itself restarts. Process exit reclaims the fds; the next server start
# unlinks stale socket files before binding.
_persistent_gamepads = {}


class SelkiesGamepad:
    def __init__(self, js_interposer_socket_path, evdev_interposer_socket_path, loop=None):
        self.js_sock_path = js_interposer_socket_path
        self.evdev_sock_path = evdev_interposer_socket_path
        self.loop = loop or asyncio.get_running_loop()
        
        self.mapper = None # Set by set_config
        self.config_payload_cache = None # Cache for js_config_t

        self.js_server = None
        self.evdev_server = None
        self.js_clients = {} # {writer: {'arch_bits': bits}}
        self.evdev_clients = {} # {writer: {'arch_bits': bits}}
        
        # Bounded so a single stalled gamepad client (slow writer.drain) can't let
        # this queue grow without limit and wedge event delivery for every client.
        # On overflow we drop the OLDEST event (see send_event) — for a gamepad the
        # freshest axis/button state matters, stale samples are worthless.
        self.events_queue = asyncio.Queue(maxsize=4096)
        self.running = False
        self._event_processor_task = None

    def set_config(self, client_input_name, client_num_btns, client_num_axes):
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

    def _make_interposer_config_payload(self, js_index: int, controller_config: dict) -> bytes:
        """
        Creates the configuration payload (js_config_t) to be sent to the C interposer.
        Ensures the payload is exactly C_INTERPOSER_STRUCT_SIZE (1360 bytes).
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
            else: # Default if key missing or type is wrong
                vendor_id = 0x045e # Default Xbox vendor
            raw_product = controller_config.get("product_id")
            if isinstance(raw_product, str):
                product_id = int(raw_product, 16)
            elif isinstance(raw_product, int):
                product_id = raw_product
            else: # Default
                product_id = 0x028e # Default Xbox product
            raw_version = controller_config.get("version") # Using "version" as the key
            if isinstance(raw_version, str):
                version_id = int(raw_version, 16)
            elif isinstance(raw_version, int):
                version_id = raw_version
            else: # Default
                version_id = 0x0114 # Default Xbox version

            buttons_evdev_codes = controller_config.get("buttons", [])
            axes_evdev_codes = controller_config.get("axes", [])

            # Clamp the reported counts to the array capacity so the value packed
            # into the C js_config_t can never exceed the (truncated) btn_map /
            # axes_map array lengths, which would otherwise drive an out-of-bounds
            # read in the C interposer.
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

            # Base format string for the actual data fields
            base_struct_fmt = f"={CONTROLLER_NAME_MAX_LEN}sxHHHHH{INTERPOSER_MAX_BTNS}H{INTERPOSER_MAX_AXES}B"
            
            # Calculate size of the base structure without any explicit end padding
            size_without_explicit_end_padding = struct.calcsize(base_struct_fmt) # Should be 1353

            # Calculate how much padding is needed to reach the C struct's total size
            padding_needed = C_INTERPOSER_STRUCT_SIZE - size_without_explicit_end_padding

            if padding_needed < 0:
                logging.error(
                    f"CRITICAL STRUCT SIZE ERROR: Python base packed size ({size_without_explicit_end_padding}) "
                    f"is larger than C interposer expected size ({C_INTERPOSER_STRUCT_SIZE}). "
                    f"This means constants (MAX_BTNS, MAX_AXES, NAME_LEN) or field types/order "
                    f"differ between Python 'base_struct_fmt' and C 'js_config_t'."
                )
                return b'\0' * C_INTERPOSER_STRUCT_SIZE

            # Final format string including the calculated padding at the end
            struct_fmt = f"{base_struct_fmt}{padding_needed}x"
            
            # Verify the final Python packed size matches the C expectation
            python_final_packed_size = struct.calcsize(struct_fmt)
            if python_final_packed_size != C_INTERPOSER_STRUCT_SIZE:
                # This should ideally not be hit if padding_needed was calculated correctly
                logging.error(
                    f"CRITICAL FINAL PYTHON PACKED SIZE MISMATCH for js_config_t! "
                    f"C interposer expects: {C_INTERPOSER_STRUCT_SIZE}, "
                    f"Python struct.pack calculated final size: {python_final_packed_size} using format '{struct_fmt}'. "
                    f"This indicates an issue with padding calculation logic or the base_struct_fmt."
                )
                return b'\0' * C_INTERPOSER_STRUCT_SIZE

            logging.debug(f"Using final struct_fmt: '{struct_fmt}' for js_config, packing to size {python_final_packed_size}")

            payload_args = [
                name_bytes_for_pack,    # char name[CONTROLLER_NAME_MAX_LEN]
                vendor_id,              # uint16_t vendor
                product_id,             # uint16_t product
                version_id,             # uint16_t version
                num_actual_btns,        # uint16_t num_btns (actual count)
                num_actual_axes,        # uint16_t num_axes (actual count)
            ]
            # Add elements of the padded button map array
            payload_args.extend(padded_btn_map_for_pack) # uint16_t btn_map[INTERPOSER_MAX_BTNS]
            # Add elements of the padded axes map array
            payload_args.extend(padded_axes_map_for_pack)  # uint8_t axes_map[INTERPOSER_MAX_AXES]
            # The 'x' padding specifier in struct_fmt does not take arguments in payload_args

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
            # Report the most specific format string reached before the failure.
            current_struct_fmt = struct_fmt if struct_fmt != "undefined" else base_struct_fmt
            logging.error(f"Error packing joystick config for js{js_index} with format '{current_struct_fmt}': {e}")
            config_to_log = controller_config if 'controller_config' in locals() else {}
            logging.error(f"Controller config was: {config_to_log}")
            return b'\0' * C_INTERPOSER_STRUCT_SIZE
        except Exception as e:
            config_to_log = controller_config if 'controller_config' in locals() else {}
            logging.exception(f"Unexpected error creating interposer config payload for js{js_index} with config {config_to_log}: {e}")
            return b'\0' * C_INTERPOSER_STRUCT_SIZE

    async def _handle_interposer_client(self, reader, writer, is_evdev_socket):
        peername = writer.get_extra_info('peername') 
        socket_type_str = "EVDEV" if is_evdev_socket else "JS"
        clients_dict = self.evdev_clients if is_evdev_socket else self.js_clients
        sock_path = self.evdev_sock_path if is_evdev_socket else self.js_sock_path
        log_prefix = f"Gamepad {sock_path} Client {peername} ({socket_type_str}):"
        logger_selkies_gamepad.info(f"{log_prefix} Handler started.")

        try:
            # 1. Send config payload
            if not self.config_payload_cache:
                logger_selkies_gamepad.error(f"{log_prefix} Config payload not ready. Aborting handler.")
                return
            logger_selkies_gamepad.info(f"{log_prefix} Preparing to send config payload. Length: {len(self.config_payload_cache)}, Expected C size: {EXPECTED_C_STRUCT_SIZE}, First 16 bytes: {self.config_payload_cache[:16].hex()}")
            writer.write(self.config_payload_cache)
            await writer.drain()
            await asyncio.sleep(1)
            logger_selkies_gamepad.debug(f"{log_prefix} Sent config payload.")

            # 2. Read 1-byte architecture specifier
            arch_byte = await reader.readexactly(1)
            client_sizeof_long = struct.unpack("=B", arch_byte)[0]
            client_arch_bits = client_sizeof_long * 8
            logger_selkies_gamepad.info(f"{log_prefix} Received arch specifier: {client_sizeof_long} bytes ({client_arch_bits}-bit).")
            
            clients_dict[writer] = {'arch_bits': client_arch_bits}
            logger_selkies_gamepad.info(f"{log_prefix} Added to active list. Total {socket_type_str} clients: {len(clients_dict)}.")

            # Keep connection alive
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

    async def _run_single_server(self, interposer_socket_path, is_evdev_socket):
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

    async def run_servers(self):
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

    def send_event(self, client_event_idx, client_value, is_button_event):
        if not self.mapper or not self.running:
            return
        event_package = self.mapper.get_mapped_events(client_event_idx, client_value, is_button_event)
        if event_package:
            logger_selkies_gamepad.debug(f"Gamepad {self.js_sock_path}: Queuing event: {event_package}")
            try:
                self.events_queue.put_nowait(event_package)
            except asyncio.QueueFull:
                # Drop the oldest queued event to make room for this newer one, so a
                # stalled client draining slowly can't back-pressure into unbounded
                # growth. The shutdown sentinel (None) is re-enqueued if evicted.
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

    async def _process_event_queue(self):
        logger_selkies_gamepad.info(f"Gamepad {self.js_sock_path}: Event processor started.")
        while self.running:
            try:
                event_package = await self.events_queue.get()
                if event_package is None: # Sentinel for shutdown
                    self.events_queue.task_done()
                    break
                
                logger_selkies_gamepad.debug(f"Gamepad {self.js_sock_path}: Dequeued event: {event_package}")
                
                js_data = event_package.get('js_event_data')
                evdev_template = event_package.get('evdev_event_template') 

                # Send to JS clients
                if js_data:
                    for i, (writer, client_info) in enumerate(list(self.js_clients.items())):
                        if not writer.is_closing():
                            try:
                                writer.write(js_data)
                                await writer.drain()
                                logger_selkies_gamepad.debug(f"Gamepad {self.js_sock_path}: JS event drained to client #{i}.")
                            except (ConnectionResetError, BrokenPipeError): pass 
                            except Exception as e: 
                                logger_selkies_gamepad.error(f"Error sending to JS client #{i}: {e}", exc_info=True) 
                
                # Send to EVDEV clients
                if evdev_template:
                    ev_type, ev_code, ev_value = evdev_template
                    for i, (writer, client_info) in enumerate(list(self.evdev_clients.items())):
                        if not writer.is_closing():
                            try:
                                client_arch_bits = client_info.get('arch_bits', 64) 
                                evdev_data = get_evdev_events_packed(ev_type, ev_code, ev_value, client_arch_bits)
                                writer.write(evdev_data)
                                await writer.drain()
                                logger_selkies_gamepad.debug(f"Gamepad {self.js_sock_path}: EVDEV event drained to client #{i}.")
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


    async def close(self):
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
        
        logger_selkies_gamepad.info("Gamepad services fully closed.")


# --- WebRTCInput Class ---
class WebRTCInputError(Exception): pass

class WebRTCInput:
    def __init__(
        self,
        rtc_app,
        uinput_mouse_socket_path="",
        js_socket_path_prefix="/tmp", 
        enable_clipboard="",
        enable_binary_clipboard="",
        enable_cursors=True,
        cursor_size=16, 
        cursor_scale=1.0,
        cursor_debug=False,
        max_cursor_size=32,
        data_server_instance=None,
        upload_dir=None,
        is_wayland=False,
        wayland_socket_index=0,
    ):
        self.wayland_socket_index = wayland_socket_index
        self.active_shortcut_modifiers = set()
        self.SHORTCUT_MODIFIER_XKEY_NAMES = {
            'Control_L', 'Control_R', 
            'Alt_L', 'Alt_R', 
            'Super_L', 'Super_R',
            'Meta_L', 'Meta_R'
        }
        self.active_modifiers = set()
        self.atomically_typed_keys = set()
        self.translated_keys = set() # Track translated keysyms
        self.ACTION_MODIFIER_KEYSYMS = {65507, 65508, 65513, 65514, 65511, 65512,
                                        65515, 65516, 65517, 65518}
        self.MODIFIER_KEYSYMS = {
            65505, 65506,  # Shift_L, Shift_R
            65507, 65508,  # Control_L, Control_R
            65513, 65514,  # Alt_L, Alt_R
            65027,        # ISO_Level3_Shift (AltGr)
            65511, 65512,  # Meta_L, Meta_R
            # The client maps the Meta/Windows key to Super, not Meta, so Super
            # (and Hyper) must be treated as modifiers too — otherwise they get
            # armed for auto-repeat and misrouted as ordinary keys.
            65515, 65516,  # Super_L, Super_R
            65517, 65518,  # Hyper_L, Hyper_R
        }
        self.rtc_app = rtc_app
        self.loop = asyncio.get_running_loop()
        self.js_socket_path_prefix = js_socket_path_prefix
        self.num_gamepads = 4 
        self.gamepad_instances = {}
        self.client_gamepad_associations = {} 

        self.clipboard_running = False
        # Serializes update_binary_clipboard_setting so its cancel+reassign of the
        # monitor task is atomic (two rapid toggles can't interleave at the await).
        self._binary_clipboard_lock = asyncio.Lock()
        # Singleton guard: only one start_clipboard poll loop may run at a time.
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
        # An explicit cursor_size raises the remote-cursor capture cap so the
        # requested size survives the transport instead of being resized down.
        if isinstance(cursor_size, int) and cursor_size > 0:
            max_cursor_size = max(max_cursor_size, cursor_size)
        self.max_cursor_size = max_cursor_size
        self.system_dpi = 96.0
        self.cursor_size_cap = max_cursor_size
        self.keyboard = None
        self.mouse = None
        self.xdisplay = None
        self.button_mask = 0
        self.last_x = -1
        self.last_y = -1
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
        self.on_set_enable_resize = lambda enable_resize, res: logger_webrtc_input.warning("unhandled on_set_enable_resize")
        self.on_client_fps = lambda fps: logger_webrtc_input.warning("unhandled on_client_fps")
        self.on_client_latency = lambda latency: logger_webrtc_input.warning("unhandled on_client_latency")
        self.on_resize = lambda res, display_id="primary": logger_webrtc_input.warning("unhandled on_resize")
        self.on_scaling_ratio = lambda res: logger_webrtc_input.warning("unhandled on_scaling_ratio")
        self.on_ping_response = lambda latency: logger_webrtc_input.warning("unhandled on_ping_response")
        self.on_cursor_change = self._on_cursor_change
        self.on_client_webrtc_stats = lambda webrtc_stat_type, webrtc_stats: logger_webrtc_input.warning("unhandled on_client_webrtc_stats")
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
        # Keysym policy lives here, not in the compositor: built lazily from the
        # compositor's keymap, retried on a cooldown if that read fails.
        self._wl_keymap_owner = None
        self._wl_keymap_owner_lock = asyncio.Lock()
        self._wl_keymap_retry_at = 0.0
        self.use_clipboard_fallback = False
        self.clipboard_paused = False
        # Change-detection baseline shared by the monitor AND write_clipboard: content
        # this server just wrote must never be re-broadcast (client<->server echo loop),
        # and the baseline survives client reconnects so nothing is resent unchanged.
        self._clipboard_last_bytes = None
        self._x11_clipboard_monitor = None
        self._wl_clipboard_watch_proc = None
        # Debounce REQUEST_CLIPBOARD (Ctrl/Cmd+C): coalesce bursts so we don't spawn
        # an xclip/wl-paste read per keypress (fork storm). Keyed per requesting
        # connection so one client's copy can't suppress another client's read.
        self._last_clipboard_request_ts = {}  # conn key -> last request (monotonic)
        self._clipboard_request_debounce = 0.25  # seconds
        self.clipboard_injection_lock = asyncio.Lock()
        # Strong refs for fire-and-forget tasks: asyncio only weakly references
        # running tasks, so an unreferenced one can be garbage-collected mid-flight.
        self._bg_tasks = set()
        self.keyboard_queue = asyncio.Queue()
        self.keyboard_worker_task = None
        # Stuck-key recovery: client heartbeats each held key ('kh'); the sweep
        # auto-releases any key whose heartbeat stops (key-up lost to congestion).
        self.pressed_keys = {}            # keysym -> last heartbeat (monotonic)
        # Atomic (non-alpha) keys reaped by the sweep were never physically held, so
        # a late 'ku' would emit a spurious X11 keyup. Track them to swallow it; a
        # fresh 'kd' clears the entry (the key is live again).
        self.reaped_atomic_keys = set()
        # Cap tracked held keys so a kd-flood can't grow the dict unbounded.
        self.max_pressed_keys = 1024
        # Stale window: client heartbeats every 100ms but hidden tabs throttle to
        # >=1s, so 2.0s avoids false-releasing a held key while backgrounded.
        self.key_stale_window = 2.0       # release a key unseen for this long
        self.key_sweep_interval = 0.1
        self.key_sweep_task = None
        # Server-side key auto-repeat (X11 only). XTEST/xdotool
        # synthetic presses do NOT trigger the X server's native auto-repeat, so a held
        # key would emit a single character. We re-emit held repeatable keys here at the
        # configured rate. Disabled on Wayland: the focused app repeats held
        # virtual-keyboard keys itself via wl_keyboard repeat_info, so a server-side
        # repeat would double it.
        self.key_repeat_enabled = not self.is_wayland
        self.key_repeat_delay = 0.5        # seconds a key must stay held before repeating
        self.key_repeat_interval = 0.04    # seconds between repeats (~25 Hz)
        self.key_repeat_tick = 0.02        # repeat-loop poll period
        # Pause repeat when the held key's last heartbeat is older than this (stalled
        # stream / hidden tab). Keep >~3x the client's 100ms heartbeat.
        self.key_repeat_heartbeat_grace = 0.3
        self.key_repeat_state = {}         # keysym -> monotonic time of next due repeat
        self.key_repeat_task = None
        self.on_update_rate_control_mode = lambda mode, display_id="primary": logger_webrtc_input.warning("unhandled on_update_rate_control_mode")
        self.on_update_crf = lambda value, display_id="primary": logger_webrtc_input.warning("unhandled on_update_crf")

        if self.is_wayland:
            if shutil.which("kwin_wayland"):
                self.use_clipboard_fallback = True
                logger_webrtc_input.info("kwin_wayland detected: enabling Clipboard-Input fallback for Unicode.")

            try:
                if ScreenCapture is None:
                    raise RuntimeError("pixelflux is not installed")
                self.wayland_input = ScreenCapture()
                logger_webrtc_input.info("Wayland input injection initialized.")
            except Exception as e:
                logger_webrtc_input.error(f"Failed to initialize Wayland input: {e}")

    async def _on_clipboard_read(self, data, mime_type="text/plain"):
        await self.send_clipboard_data(data, mime_type)
    def _on_cursor_change(self, data): self.send_cursor_data(data)
    async def send_clipboard_data(self, data, mime_type="text/plain"):
        # Mode router only: each transport owns its one chunked sender
        # (SelkiesStreamingApp.send_ws_clipboard_data / RTCApp.send_clipboard_data).
        if self.rtc_app.mode == "websockets":
            await self.rtc_app.send_ws_clipboard_data(data, mime_type)
        else:
            await self.rtc_app.send_clipboard_data(data, mime_type)
    def send_cursor_data(self, data):
        if self.rtc_app.mode == "websockets": self.rtc_app.send_ws_cursor_data(data)
        else: self.rtc_app.send_cursor_data(data)

    def __keyboard_connect(self): self.keyboard = _XTestKeyboard(self.xdisplay) if self.xdisplay else None

    async def _load_server_autorepeat_rate(self):
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
            # Sanity-clamp: reject nonsense (e.g. 0) that would busy-repeat or never repeat.
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
    def __mouse_connect(self):
        if self.uinput_mouse_socket_path:
            logger_webrtc_input.info(f"Connecting to uinput mouse socket: {self.uinput_mouse_socket_path}")
            self.uinput_mouse_socket = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
        if not self.is_wayland and self.xdisplay:
            self.mouse = _XTestMouse(self.xdisplay)
    def __mouse_disconnect(self):
        if self.mouse: del self.mouse; self.mouse = None
    def __mouse_emit(self, *args, **kwargs):
        if self.uinput_mouse_socket_path:
            cmd = {"args": args, "kwargs": kwargs}
            data = msgpack.packb(cmd, use_bin_type=True)
            self.uinput_mouse_socket.sendto(data, self.uinput_mouse_socket_path)

    async def __gamepad_connect(self, gamepad_idx, client_name, client_num_btns, client_num_axes):
        if not (0 <= gamepad_idx < self.num_gamepads):
            logger_webrtc_input.error(f"Client association: Gamepad index {gamepad_idx} out of range (0-{self.num_gamepads-1}).")
            return

        if gamepad_idx not in self.gamepad_instances:
            logger_webrtc_input.error(
                f"Client association: No persistent gamepad instance found for index {gamepad_idx}. "
                f"This should not happen if _initialize_persistent_gamepads ran correctly."
            )
            return

        # Log the association
        logger_webrtc_input.info(
            f"Client controller '{client_name}' ({client_num_btns}b, {client_num_axes}a) "
            f"is now associated with persistent virtual gamepad slot {gamepad_idx}."
        )
        
        self.client_gamepad_associations[gamepad_idx] = {
            "client_name": client_name,
            "client_num_btns": client_num_btns,
            "client_num_axes": client_num_axes,
            "association_time": time.time()
        }

    async def __gamepad_disconnect(self, gamepad_idx=None):
        if gamepad_idx is None: # Disassociate all if no specific index
            indices_to_disassociate = list(self.client_gamepad_associations.keys())
            logger_webrtc_input.info("Disassociating all client gamepads from persistent slots.")
        elif not (0 <= gamepad_idx < self.num_gamepads):
            logger_webrtc_input.error(f"Client disassociation: Gamepad index {gamepad_idx} out of range.")
            return
        else:
            indices_to_disassociate = [gamepad_idx]

        for idx in indices_to_disassociate:
            if idx in self.client_gamepad_associations:
                associated_info = self.client_gamepad_associations.pop(idx)
                logger_webrtc_input.info(
                    f"Client controller '{associated_info.get('client_name', 'Unknown')}' "
                    f"disassociated from persistent virtual gamepad slot {idx}."
                )
            elif gamepad_idx is not None: # Only log if a specific, non-associated index was requested
                 logger_webrtc_input.warning(
                    f"Client disassociation: No active client association found for gamepad slot {idx} to disassociate."
                )

    def __gamepad_emit_btn(self, gamepad_idx, client_btn_num, client_btn_val):
        gamepad = self.gamepad_instances.get(gamepad_idx)
        if gamepad:
            gamepad.send_event(client_btn_num, client_btn_val, is_button_event=True)

    def __gamepad_emit_axis(self, gamepad_idx, client_axis_num, client_axis_val):
        gamepad = self.gamepad_instances.get(gamepad_idx)
        if gamepad:
            gamepad.send_event(client_axis_num, client_axis_val, is_button_event=False)
            
    async def connect(self):
        if not self.is_wayland and X11_LIBS_AVAILABLE:
            try: self.xdisplay = display.Display()
            except Exception as e: logger_webrtc_input.error(f"Failed to connect to X display: {e}"); self.xdisplay = None
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
        
        # Initialize persistent gamepad instances
        await self._initialize_persistent_gamepads()

        if self.is_wayland:
            self.keyboard_worker_task = asyncio.create_task(self._keyboard_worker())
        if self.key_sweep_task is None:
            self.key_sweep_task = asyncio.create_task(self._key_stale_sweep())
        if self.key_repeat_enabled and self.key_repeat_task is None:
            self.key_repeat_task = asyncio.create_task(self._key_repeat_loop())

    async def _initialize_persistent_gamepads(self):
        logger_webrtc_input.info(f"Initializing {self.num_gamepads} persistent gamepad instances...")
        if not os.path.exists(self.js_socket_path_prefix):
            try:
                os.makedirs(self.js_socket_path_prefix, exist_ok=True)
                logger_webrtc_input.info(f"Created directory for gamepad sockets: {self.js_socket_path_prefix}")
            except OSError as e:
                logger_webrtc_input.error(f"Failed to create directory {self.js_socket_path_prefix} for gamepad sockets: {e}")
                return # Cannot proceed if directory creation fails

        for i in range(self.num_gamepads):
            if i in self.gamepad_instances: # Should not happen on initial call but good for robustness
                logger_webrtc_input.warning(f"Gamepad instance for index {i} already exists. Skipping re-initialization.")
                continue

            # Adopt the process-wide instance when one is live: its sockets are what
            # already-running apps hold open, so a service restart (transport mode
            # switch) must reuse them — rebinding would orphan those apps' fds.
            existing = _persistent_gamepads.get(i)
            if existing is not None and existing.running:
                self.gamepad_instances[i] = existing
                logger_webrtc_input.info(
                    f"Adopted live persistent gamepad instance for index {i} (JS: {existing.js_sock_path})."
                )
                continue

            js_ip_sock_path = os.path.join(self.js_socket_path_prefix, f"selkies_js{i}.sock")
            evdev_ip_sock_path = os.path.join(self.js_socket_path_prefix, f"selkies_event{1000+i}.sock")

            gamepad = SelkiesGamepad(js_ip_sock_path, evdev_ip_sock_path, self.loop)

            # Use standardized name and capabilities from STANDARD_XPAD_CONFIG
            gamepad_name_for_interposer = STANDARD_XPAD_CONFIG.get("name", f"Selkies Virtual Gamepad {i}")
            std_num_btns = len(STANDARD_XPAD_CONFIG["btn_map"])
            std_num_axes = len(STANDARD_XPAD_CONFIG["axes_map"])

            # Pass the standardized name to set_config.
            gamepad.set_config(gamepad_name_for_interposer, std_num_btns, std_num_axes)

            self._spawn_task(gamepad.run_servers())
            _persistent_gamepads[i] = gamepad
            self.gamepad_instances[i] = gamepad # Store by index i
            logger_webrtc_input.info(f"Initialized and started persistent gamepad instance for index {i} (Name: '{gamepad_name_for_interposer}', JS: {js_ip_sock_path}, EVDEV: {evdev_ip_sock_path}).")

    async def disconnect(self):
        # The persistent gamepad instances (and their interposer sockets) outlive
        # this handler: apps hold those fds open across client sessions AND
        # transport mode switches, and would go permanently silent if the sockets
        # closed here. Only the per-session client->slot associations are ours.
        logger_webrtc_input.info("Releasing gamepad associations (persistent instances stay up).")
        await self.__gamepad_disconnect()
        self.gamepad_instances = {}
        self.__mouse_disconnect()
        if self.xdisplay: self.xdisplay = None
        
        if self.keyboard_worker_task:
            self.keyboard_worker_task.cancel()
            self.keyboard_worker_task = None
        if self.key_sweep_task:
            self.key_sweep_task.cancel()
            self.key_sweep_task = None
        if self.key_repeat_task:
            self.key_repeat_task.cancel()
            self.key_repeat_task = None
        self.pressed_keys.clear()
        self.reaped_atomic_keys.clear()
        self.key_repeat_state.clear()
        # Drop any half-received multipart clipboard so a new connection can't inherit it.
        self._reset_multipart_clipboard()

    async def _key_stale_sweep(self):
        """Auto-release keys whose heartbeats stopped (lost key-up), so a key is
        never stuck held when the network drops packets."""
        try:
            while True:
                await asyncio.sleep(self.key_sweep_interval)
                if not self.pressed_keys:
                    continue
                now = time.monotonic()
                stale = [k for k, seen in self.pressed_keys.items() if now - seen > self.key_stale_window]
                for keysym in stale:
                    # Re-check: a heartbeat or re-press during a prior await may
                    # have refreshed this key, so don't release a now-live key.
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
                        # Remember it so a late, legit 'ku' is swallowed instead of
                        # injecting a spurious X11 keyup. Bound the set like
                        # pressed_keys so it can't grow without limit.
                        if len(self.reaped_atomic_keys) < self.max_pressed_keys:
                            self.reaped_atomic_keys.add(keysym)
                        continue
                    try:
                        if self.is_wayland:
                            # Wayland injection is serialized through keyboard_queue;
                            # no shared X11 state is touched across an await here.
                            self.active_modifiers.discard(keysym)
                            self.atomically_typed_keys.discard(keysym)
                            self.keyboard_queue.put_nowait(("ku", keysym))
                        else:
                            # X11 injection isn't queue-serialized: defer the modifier/atomic
                            # state discard until after the release so a concurrent kd still
                            # sees correct active_modifiers. The keysym was popped above, so a
                            # non-None entry here means a concurrent kd re-pressed it. That kd
                            # already injected its own keydown, so on any re-press we MUST NOT
                            # re-inject a down (would double-press); we just abandon our release.
                            if self.pressed_keys.get(keysym) is not None:
                                # kd raced us before the keyup: skip both injections; the kd
                                # owns the key (and its modifier/atomic state) now.
                                continue
                            await self.send_x11_keypress(keysym, down=False)
                            if self.pressed_keys.get(keysym) is not None:
                                # kd raced us during the keyup await: it already re-pressed
                                # the key, so abandon the heal (no down) and leave its
                                # modifier/atomic state intact for the live press.
                                continue
                            self.active_modifiers.discard(keysym)
                            self.atomically_typed_keys.discard(keysym)
                    except Exception as e:
                        logger_webrtc_input.warning(f"Failed to auto-release key {keysym}: {e}")
        except asyncio.CancelledError:
            pass

    async def _key_repeat_loop(self):
        """X11 server-side key auto-repeat for held keys.

        XTEST/xdotool synthetic presses don't trigger the X server's native
        auto-repeat, so without this a held key types a single character. We re-emit the
        most-recently-pressed still-held repeatable key at key_repeat_interval after an
        initial key_repeat_delay -- exactly like a physical keyboard, where only the last
        key pressed repeats and releasing it resumes the previously-held one. Modifiers
        and atomically-typed (single-shot) keys are never armed, so the repeated key
        carries whatever modifiers are currently held (Shift+Arrow selection,
        Ctrl+Backspace word-delete, Ctrl+Z, etc. all repeat like native -- no special
        shortcut suppression). Repeats are KeyPress-only (no synthetic KeyRelease),
        matching X11 detectable auto-repeat, so state-based games keep the key held with
        no movement stutter and ignore the extra presses. Wayland is excluded (the
        focused app repeats held virtual-keyboard keys itself via wl_keyboard
        repeat_info, so a server-side repeat would double it)."""
        try:
            while True:
                await asyncio.sleep(self.key_repeat_tick)
                if not self.key_repeat_state:
                    continue
                # Drop keys released/reaped since arming (mirrors the stale-sweep race
                # guard, so we never inject a down after the key-up).
                for k in [k for k in self.key_repeat_state if k not in self.pressed_keys]:
                    self.key_repeat_state.pop(k, None)
                if not self.key_repeat_state:
                    continue
                # Only the newest still-held key repeats: key_repeat_state is
                # insertion-ordered and arming moves a key to the end, so the last entry
                # is the most recent. Releasing it lets the previous key resume next tick.
                keysym = next(reversed(self.key_repeat_state))
                now = time.monotonic()
                if now < self.key_repeat_state[keysym]:
                    continue
                # Heartbeats stopped (stalled stream or hidden tab): pause repeat until
                # they resume. The key stays held, bounding run-on to ~grace instead of
                # the whole stale window.
                last_seen = self.pressed_keys.get(keysym)
                if last_seen is None or (now - last_seen) > self.key_repeat_heartbeat_grace:
                    continue
                try:
                    if keysym in self.atomically_typed_keys:
                        # This key was typed once atomically (digit/punctuation) rather
                        # than as a physical KeyPress. Repeat it in-process via XTEST at
                        # the right shift level instead of re-forking `xdotool type` every
                        # tick. A self-contained press+release (not a bare KeyPress) is
                        # used because the 'ku' path injects no key-up for atomic keys, so
                        # a lone press would leave the key stuck down. Fall back to the
                        # co,end path when the keysym has no keycode in the layout or sits
                        # only on an AltGr level.
                        injected = False
                        if self.keyboard is not None:
                            try:
                                # Repeat via the XTEST shim, which resolves the keymap
                                # level and synthesizes Shift/AltGr (and overlay-binds
                                # unmapped keysyms). A self-contained press+release since
                                # the 'ku' path injects no key-up for atomic keys.
                                self.keyboard.press(keysym)
                                self.keyboard.release(keysym)
                                injected = True
                            except Exception as e:
                                logger_webrtc_input.debug(
                                    f"XTEST atomic repeat failed for keysym {keysym}; falling back: {e}"
                                )
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
                # A 'ku' during the await released the key: stop repeating it (the extra
                # down we just sent is healed by the real key-up / stale-sweep).
                if keysym in self.pressed_keys:
                    self.key_repeat_state[keysym] = time.monotonic() + self.key_repeat_interval
                else:
                    self.key_repeat_state.pop(keysym, None)
        except asyncio.CancelledError:
            pass

    async def reset_keyboard(self):
        if self.is_wayland:
            if self.wayland_input:
                # Release common modifiers
                # Ctrl, Shift, Alt, AltGr, Meta, Super, Hyper (the client maps the
                # Meta/Windows key to Super, so Super must be released here too).
                modifiers = [65507, 65505, 65513, 65508, 65506, 65027,
                             65511, 65512, 65515, 65516, 65517, 65518]
                owner = self._wl_keymap_owner
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
            # Release any in-flight translated (Cyrillic) keys before clearing the
            # map, else the injected QWERTY key stays down after reset. The original
            # keysym down=False routes through send_x11_keypress's translation.
            for k in list(self.translated_keys):
                try: await self.send_x11_keypress(k, down=False)
                except Exception: pass
            # Release every still-held key (not just modifiers/hotkeys): a held
            # non-modifier key (e.g. 'w' in a game) would otherwise stay pressed
            # in the compositor forever once pressed_keys is cleared below, since
            # the stale-sweep is the only other releaser and it never runs after
            # the clear. Atomically-typed (single-shot) keys were never physically
            # held, so skip them.
            for keysym in list(self.pressed_keys):
                if keysym in self.atomically_typed_keys:
                    continue
                try: await self.send_x11_keypress(keysym, down=False)
                except Exception as e: logger_webrtc_input.warning(f"Error releasing held key {keysym}: {e}")
            # Clear server-side key state so a reset can't leave stuck-modifier desync.
            self.active_modifiers.clear()
            self.active_shortcut_modifiers.clear()
            self.atomically_typed_keys.clear()
            self.translated_keys.clear()
            # Drop heartbeat-tracked keys too; otherwise the stale-sweep would
            # auto-release a key the reset just cleared (mirrors disconnect()).
            self.pressed_keys.clear()
            self.reaped_atomic_keys.clear()
            return

        if not self.keyboard or not self.xdisplay :
            logger_webrtc_input.warning("Cannot reset keyboard, X display or keyboard controller not available.")
            return
        logger_webrtc_input.info("Resetting keyboard modifiers.")
        lctrl, lshift, lalt, rctrl, rshift, ralt = 65507, 65505, 65513, 65508, 65506, 65027
        lmeta, rmeta, keyf, keyF, keym, keyM, escape = 65511, 65512, 102, 70, 109, 77, 65307
        # Super/Hyper included: the client maps the Meta/Windows key to Super.
        lsuper, rsuper, lhyper, rhyper = 65515, 65516, 65517, 65518
        for k in [lctrl, lshift, lalt, rctrl, rshift, ralt, lmeta, rmeta,
                  lsuper, rsuper, lhyper, rhyper, keyf, keyF, keym, keyM, escape]:
            try: await self.send_x11_keypress(k, down=False)
            except Exception as e: logger_webrtc_input.warning(f"Error resetting key {k}: {e}")
        # Release every still-held key, not just the modifier/hotkey list above: a
        # held non-modifier key (e.g. 'w' in a game) would otherwise stay pressed in
        # the X server forever once pressed_keys is cleared below (the stale-sweep,
        # the only other releaser, never runs after the clear). Atomically-typed
        # (single-shot, non-alpha) keys were never physically held on X11, so skip
        # them to avoid a spurious keyup.
        for keysym in list(self.pressed_keys):
            if keysym in self.atomically_typed_keys:
                continue
            try: await self.send_x11_keypress(keysym, down=False)
            except Exception as e: logger_webrtc_input.warning(f"Error releasing held key {keysym}: {e}")
        # Release any in-flight translated (Cyrillic) keys before clearing the map,
        # else the injected QWERTY key stays physically down after reset. Passing the
        # original keysym down=False makes send_x11_keypress emit the translated keyup.
        for k in list(self.translated_keys):
            try: await self.send_x11_keypress(k, down=False)
            except Exception as e: logger_webrtc_input.warning(f"Error releasing translated key {k}: {e}")
        # Clear server-side key state (after the release loop consumes it) to avoid stuck-modifier desync.
        self.active_modifiers.clear()
        self.active_shortcut_modifiers.clear()
        self.atomically_typed_keys.clear()
        self.translated_keys.clear()
        # Drop heartbeat-tracked keys too; otherwise the stale-sweep would
        # auto-release a key the reset just cleared (mirrors disconnect()).
        self.pressed_keys.clear()
        self.reaped_atomic_keys.clear()
        self.key_repeat_state.clear()

    def send_mouse(self, action, data):
        if action == MOUSE_POSITION:
            if self.mouse: self.mouse.position = data
        elif action == MOUSE_MOVE:
            x, y = data
            if self.uinput_mouse_socket_path:
                self.__mouse_emit(UINPUT_REL_X, x, syn=False)
                self.__mouse_emit(UINPUT_REL_Y, y)
            elif self.xdisplay:
                xtest.fake_input(self.xdisplay, Xlib.X.MotionNotify, detail=True, root=Xlib.X.NONE, x=x, y=y)
                # flush() (send, no round-trip) suffices — XTEST needs no reply; sync()
                # would add a blocking server round-trip on every mouse move.
                self.xdisplay.flush()
        elif action == MOUSE_SCROLL_UP:
            # The XTest/Wayland backends (the defaults) map this action to a physical
            # wheel-DOWN (button 5 / REL_WHEEL -1) — the MOUSE_SCROLL_* constants are
            # named for the client button, not the physical direction. uinput must
            # match, so REL_WHEEL is -1 here, not +1 (which scrolled the wrong way).
            if self.uinput_mouse_socket_path: self.__mouse_emit(UINPUT_REL_WHEEL, -1)
            elif self.mouse: self.mouse.scroll(0, -1)
        elif action == MOUSE_SCROLL_DOWN:
            if self.uinput_mouse_socket_path: self.__mouse_emit(UINPUT_REL_WHEEL, 1)
            elif self.mouse: self.mouse.scroll(0, 1)
        elif action == MOUSE_SCROLL_LEFT:
            if self.mouse: self.mouse.scroll(-1, 0)
        elif action == MOUSE_SCROLL_RIGHT:
            if self.mouse: self.mouse.scroll(1, 0)
        elif action == MOUSE_BUTTON: 
            btn_map_key = "uinput" if self.uinput_mouse_socket_path else "x11"
            btn_uinput_or_x11 = MOUSE_BUTTON_MAP[data[1]][btn_map_key]
            if data[0] == MOUSE_BUTTON_PRESS:
                if self.uinput_mouse_socket_path: self.__mouse_emit(btn_uinput_or_x11, 1)
                elif self.mouse: self.mouse.press(btn_uinput_or_x11)
            else:
                if self.uinput_mouse_socket_path: self.__mouse_emit(btn_uinput_or_x11, 0)
                elif self.mouse: self.mouse.release(btn_uinput_or_x11)

    async def send_x11_keypress(self, keysym, down=True):
        if down:
            if (self.active_modifiers & self.ACTION_MODIFIER_KEYSYMS) and keysym in CYRILLIC_TO_QWERTY_KEYSYM:
                self.translated_keys.add(keysym)
                keysym = CYRILLIC_TO_QWERTY_KEYSYM[keysym]
        else:
            if keysym in self.translated_keys:
                self.translated_keys.discard(keysym)
                keysym = CYRILLIC_TO_QWERTY_KEYSYM[keysym]

        if self.is_wayland and self.wayland_input:
            # Wayland keymap contract: selkies owns keysym policy; the compositor
            # exposes only inject_key + set_keymap_string. Layout keys resolve with
            # Shift/AltGr synthesis; anything the layout lacks — the Unicode plane,
            # Euro, IME output — binds to a dynamic overlay keycode. Every keysym
            # flows ordered through the one compositor channel; wtype/clipboard
            # below are error fallbacks.
            owner = await self._ensure_wayland_keymap_owner()
            if owner is not None:
                try:
                    if down:
                        owner.press(keysym)
                    else:
                        owner.release(keysym)
                    return
                except Exception as e:
                    logger_webrtc_input.warning(
                        f"Wayland keymap injection failed for keysym {keysym}; falling back: {e}"
                    )
            await self._xdotool_fallback(keysym, down)
            return

        is_printable = (0x20 <= keysym <= 0xFF) or ((keysym & 0xFF000000) == 0x01000000)
        action = "keydown" if down else "keyup"
        command = None
        use_keyboard_for_printable = False
        # Whether this keysym may be injected via in-process XTEST instead of a
        # per-event xdotool fork. Not eligible for the --clearmodifiers case (XTEST
        # can't force the base keysym while a modifier is held), so that stays on
        # xdotool below.
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
                        # Type via in-process XTEST, which resolves the keymap level and
                        # synthesizes Shift/AltGr as needed -- no per-character xdotool
                        # fork (and no silent drop when a fork fails). Falls back to
                        # xdotool below if the keysym can't be resolved.
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
            # Fast path: for keysyms that resolve to a keycode in the current keymap,
            # inject directly through the already-open display via XTEST. This avoids a
            # ~15ms xdotool subprocess fork on every shortcut/arrow/function key. Falls
            # through to the xdotool subprocess below when there is no keycode (e.g. a
            # keysym absent from the current layout that xdotool can still synthesize).
            if allow_xtest and xtest is not None and self.xdisplay is not None:
                try:
                    keycode = self.xdisplay.keysym_to_keycode(keysym)
                    if keycode:
                        xtest.fake_input(
                            self.xdisplay,
                            X.KeyPress if down else X.KeyRelease,
                            keycode,
                        )
                        self.xdisplay.flush()
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
            except Exception:
                pass

        if use_keyboard_for_printable or not command:
            try:
                if not self.keyboard:
                    await self._xdotool_fallback(keysym, down)
                    return

                # Inject the printable keysym through the bundled Xlib XTEST
                # keyboard shim; on an unmapped keysym it raises and we fall
                # back to xdotool below.
                if down:
                    self.keyboard.press(keysym)
                else:
                    self.keyboard.release(keysym)
            except Exception:
                await self._xdotool_fallback(keysym, down)

    def _type_text_xtest(self, text):
        """Type a string in-process via the XTEST shim: each char as a press+release
        of its keysym (mapped -> shift-synthesized; unmapped -> spare-keycode
        overlay). Returns True on full success, False (having typed nothing) if the
        shim is unavailable or any char can't be resolved, so the caller can fall
        back to xdotool without double-typing."""
        if not self.keyboard or not text:
            return False
        # Pre-resolve every char so a mid-string failure doesn't type a partial line.
        keysyms = []
        for ch in text:
            cp = ord(ch)
            keysyms.append(cp if 0x20 <= cp <= 0xFF else (0x01000000 | cp))
        try:
            for ks in keysyms:
                self.keyboard.press(ks)
                self.keyboard.release(ks)
            return True
        except Exception as e:
            logger_webrtc_input.debug(f"in-process type failed ({e}); falling back to xdotool")
            return False


    def _spawn_task(self, coro, name=None):
        """create_task with a keep-alive reference and error logging."""
        task = asyncio.create_task(coro, name=name)
        self._bg_tasks.add(task)

        def _done(t):
            self._bg_tasks.discard(t)
            if not t.cancelled() and t.exception() is not None:
                logger_webrtc_input.error(f"Background task {t.get_name()} failed: {t.exception()}")

        task.add_done_callback(_done)
        return task

    async def _ensure_wayland_keymap_owner(self):
        """Get-or-build the keymap owner. Reading the compositor keymap blocks
        (bounded) and compiling it costs milliseconds, so both run off the loop;
        after that, press/release are sync dict work + channel sends."""
        if self._wl_keymap_owner is not None:
            return self._wl_keymap_owner
        if not hasattr(self.wayland_input, 'set_keymap_string'):
            return None
        now = time.monotonic()
        if now < self._wl_keymap_retry_at:
            return None
        async with self._wl_keymap_owner_lock:
            if self._wl_keymap_owner is not None:
                return self._wl_keymap_owner
            loop = asyncio.get_running_loop()
            try:
                def _build():
                    text = self.wayland_input.get_xkb_keymap_string()
                    return _WaylandKeymapOwner(self.wayland_input, text)
                self._wl_keymap_owner = await loop.run_in_executor(None, _build)
                logger_webrtc_input.info(
                    f"Wayland keymap owner ready ({len(self._wl_keymap_owner._map)} keysyms).")
            except Exception as e:
                self._wl_keymap_retry_at = time.monotonic() + 5.0
                logger_webrtc_input.warning(
                    f"Wayland keymap owner unavailable ({e}); retrying in 5s.")
            return self._wl_keymap_owner

    async def _xdotool_fallback(self, keysym_number, down=True):
        if self.is_wayland:
            if not down:
                return
            char_to_type = None
            if (keysym_number & 0xFF000000) == 0x01000000:
                unicode_codepoint = keysym_number & 0x00FFFFFF
                if 0 <= unicode_codepoint <= 0x10FFFF:
                    try:
                        char_to_type = chr(unicode_codepoint)
                    except ValueError:
                        pass
            elif 0x20 <= keysym_number <= 0xFF:
                try:
                    char_to_type = chr(keysym_number)
                except ValueError:
                    pass
            elif keysym_number == 0x20AC:
                char_to_type = '€'
            else:
                if libxkb is not None:
                    try:
                        buf = ctypes.create_string_buffer(8)
                        res = libxkb.xkb_keysym_to_utf8(keysym_number, buf, 8)
                        if res > 0:
                            char_to_type = buf.value.decode('utf-8')
                    except Exception:
                        pass

                if not char_to_type and XK is not None:
                    try:
                        keysym_name = XK.keysym_to_string(keysym_number)
                        if keysym_name and len(keysym_name) == 1:
                            char_to_type = keysym_name
                    except Exception:
                        pass

            if char_to_type:
                if getattr(self, 'use_clipboard_fallback', False):
                    await self._inject_unicode_via_clipboard(char_to_type)
                    return
                try:
                    command_wtype = ["wtype", "--", char_to_type]
                    process_wtype = await subprocess.create_subprocess_exec(
                        *command_wtype,
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                        env=self._get_wl_env()
                    )
                    try:
                        await asyncio.wait_for(process_wtype.communicate(), timeout=1.0)
                    except asyncio.TimeoutError:
                        try:
                            process_wtype.kill()
                        except ProcessLookupError:
                            pass
                        await process_wtype.wait()
                        raise
                except Exception as e:
                    logger_webrtc_input.warning(f"wtype fallback failed: {e}")

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
                if 0x20 <= keysym_number <= 0x7E or keysym_number >= 0xA0:
                    try:
                        keysym_name_from_xlib = chr(keysym_number)
                        char_for_type_cmd_fallback = keysym_name_from_xlib
                    except ValueError:
                        return
                else:
                    return
            else:
                if len(keysym_name_from_xlib) == 1:
                    char_for_type_cmd_fallback = keysym_name_from_xlib
            
            xdotool_key_arg = keysym_name_from_xlib

            if len(keysym_name_from_xlib) == 1:
                char_code = ord(keysym_name_from_xlib)
                if char_code >= 0x80 or (char_code == keysym_number and char_code != 0x00):
                    xdotool_key_arg = f"U{char_code:04X}"
            elif keysym_number == 0x00a3: # XK_sterling
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
                    command_type = ["xdotool", "type", "--clearmodifiers", char_to_type]
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

    async def _inject_unicode_via_clipboard(self, text_to_type):
        async with self.clipboard_injection_lock:
            self.clipboard_paused = True
            KEY_SHIFT_L = 0xFFE1
            KEY_INSERT  = 0xFF63

            currently_active_mods = list(self.active_modifiers)

            try:
                for mod_keysym in currently_active_mods:
                    await self.send_x11_keypress(mod_keysym, down=False)

                old_data, old_mime = await self.read_clipboard(use_binary=True)

                mime_to_use = "UTF8_STRING" if not self.is_wayland else "text/plain"
                await self.write_clipboard(text_to_type, mime_type=mime_to_use)
                await asyncio.sleep(0.02)

                await self.send_x11_keypress(KEY_SHIFT_L, down=True)
                await self.send_x11_keypress(KEY_INSERT, down=True)
                await self.send_x11_keypress(KEY_INSERT, down=False)
                await self.send_x11_keypress(KEY_SHIFT_L, down=False)
                await asyncio.sleep(0.05)

                if old_data is not None:
                    await self.write_clipboard(old_data, mime_type=old_mime or "text/plain")
                elif self.is_wayland:
                    try:
                        proc = await subprocess.create_subprocess_exec(
                            "wl-copy", "--clear",
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                            env=self._get_wl_env()
                        )
                        await self._communicate_or_kill(proc, 1.0, "wl-copy --clear")
                    except Exception:
                        pass

            except Exception as e:
                logger_webrtc_input.error(f"Error during clipboard injection: {e}", exc_info=True)
            finally:
                for mod_keysym in currently_active_mods:
                    if mod_keysym in self.active_modifiers:
                        await self.send_x11_keypress(mod_keysym, down=True)

                self.clipboard_paused = False

    async def send_x11_mouse(self, x, y, button_mask, scroll_magnitude, relative=False, display_id='primary'):
        # Clamp client-controlled magnitude so the X11 scroll loop can't block the event loop (DoS).
        try:
            scroll_magnitude = max(0, min(int(scroll_magnitude), 64))
        except (TypeError, ValueError):
            scroll_magnitude = 0
        if relative:
            final_x = self.last_x + x
            final_y = self.last_y + y
        else:
            offset_x = 0
            offset_y = 0
            if self.data_server_instance and hasattr(self.data_server_instance, 'display_layouts'):
                layout = self.data_server_instance.display_layouts.get(display_id) 
                if layout:
                    offset_x = layout.get('x', 0) 
                    offset_y = layout.get('y', 0)
            final_x = x + offset_x
            final_y = y + offset_y

        position_changed = (final_x != self.last_x or final_y != self.last_y)
        self.last_x = final_x
        self.last_y = final_y

        if self.wayland_input:
            is_static_relative = relative and (x == 0 and y == 0)
            
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

                        if bit_index == 0: # Left
                            self.wayland_input.inject_mouse_button(272, state)
                        elif bit_index == 1: # Middle
                            self.wayland_input.inject_mouse_button(274, state)
                        elif bit_index == 2: # Right
                            self.wayland_input.inject_mouse_button(273, state)
                        
                        elif bit_index == 3:
                            if scroll_magnitude > 0: 
                                if is_pressed_now:
                                    self.wayland_input.inject_mouse_scroll(0.0, 10.0 * mag)
                            else:
                                if is_pressed_now:
                                    await self.send_x11_keypress(KEYSYM_ALT_L, down=True)
                                    await self.send_x11_keypress(KEYSYM_LEFT_ARROW, down=True)
                                    await self.send_x11_keypress(KEYSYM_LEFT_ARROW, down=False)
                                    await self.send_x11_keypress(KEYSYM_ALT_L, down=False)

                        elif bit_index == 4:
                            if scroll_magnitude > 0:
                                if is_pressed_now:
                                    self.wayland_input.inject_mouse_scroll(0.0, -10.0 * mag)
                            else:
                                if is_pressed_now:
                                    await self.send_x11_keypress(KEYSYM_ALT_L, down=True)
                                    await self.send_x11_keypress(KEYSYM_RIGHT_ARROW, down=True)
                                    await self.send_x11_keypress(KEYSYM_RIGHT_ARROW, down=False)
                                    await self.send_x11_keypress(KEYSYM_ALT_L, down=False)

                        elif bit_index == 6:
                            if scroll_magnitude > 0 and is_pressed_now:
                                self.wayland_input.inject_mouse_scroll(-10.0 * mag, 0.0)
                        elif bit_index == 7:
                            if scroll_magnitude > 0 and is_pressed_now:
                                self.wayland_input.inject_mouse_scroll(10.0 * mag, 0.0)

            self.button_mask = button_mask
            return
        if relative:
            self.send_mouse(MOUSE_MOVE, (x, y))
        elif position_changed:
            self.send_mouse(MOUSE_POSITION, (final_x, final_y))
        self.last_x = final_x
        self.last_y = final_y
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
            # flush() (send, no round-trip) suffices for the injected events; sync()
            # would add a blocking server round-trip per mouse event.
            self.xdisplay.flush()
    async def update_binary_clipboard_setting(self, enabled: bool):
        """Asynchronously updates the binary clipboard setting and restarts the monitor if it's running."""
        # Hold the lock across the whole check+cancel+reassign: the await on task
        # cancellation is a suspension point, so without serialization two rapid
        # differing toggles can both pass the early-return and spawn duplicate monitors.
        async with self._binary_clipboard_lock:
            new_setting_str = "true" if enabled else "false"
            if self.enable_binary_clipboard == new_setting_str:
                return
            logger_webrtc_input.info(f"Binary clipboard setting changing to: {enabled}. Restarting monitor.")
            self.enable_binary_clipboard = new_setting_str
            if self.clipboard_monitor_task and not self.clipboard_monitor_task.done():
                self.stop_clipboard()  # Signal the loop to exit
                self.clipboard_monitor_task.cancel()
                try:
                    await self.clipboard_monitor_task
                except asyncio.CancelledError:
                    pass
                self.clipboard_monitor_task = asyncio.create_task(self.start_clipboard())
    def _get_wl_env(self):
        env = os.environ.copy()
        env["WAYLAND_DISPLAY"] = f"wayland-{self.wayland_socket_index}"
        return env

    async def _get_file(self, file_path, target_mime):
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

    async def _kill_and_reap_process(self, proc, description):
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

    async def _communicate_or_kill(self, proc, timeout, description):
        try:
            return await asyncio.wait_for(proc.communicate(), timeout=timeout)
        except asyncio.TimeoutError:
            await self._kill_and_reap_process(proc, description)
            raise

    async def _wait_or_kill(self, proc, timeout, description):
        try:
            return await asyncio.wait_for(proc.wait(), timeout=timeout)
        except asyncio.TimeoutError:
            await self._kill_and_reap_process(proc, description)
            raise

    def _clipboard_has_consumers(self):
        if getattr(self.rtc_app, "mode", None) != "websockets":
            return True
        server = self.data_server_instance or getattr(self.rtc_app, "data_streaming_server", None)
        return bool(server and getattr(server, "clients", None))

    async def read_clipboard(self, use_binary=False):
        """Reads clipboard. Native paths first (compositor cache on Wayland, the
        XFixes monitor on X11); wl-paste/xclip forks are the fallback."""
        if self.is_wayland:
            cached = getattr(self, '_wl_native_last', None)
            if cached is not None:
                raw, native_mime = cached
                if native_mime.startswith('image/'):
                    if use_binary:
                        return bytes(raw), native_mime
                else:
                    return bytes(raw).decode('utf-8', errors='replace'), 'text/plain'
            try:
                proc_types = await subprocess.create_subprocess_exec(
                    "wl-paste", "--list-types",
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                    env=self._get_wl_env()
                )
                stdout_types, _ = await self._communicate_or_kill(proc_types, 1.0, "wl-paste --list-types")
                
                if proc_types.returncode != 0:
                    return None, None

                available_types = stdout_types.decode().strip().split('\n')

                if use_binary:
                    image_mimes = ['image/png', 'image/jpeg', 'image/bmp', 'image/webp']
                    target_mime = next((m for m in image_mimes if m in available_types), None)
                    if target_mime:
                        proc_data = await subprocess.create_subprocess_exec(
                            "wl-paste", "--type", target_mime,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            env=self._get_wl_env()
                        )
                        stdout_data, _ = await self._communicate_or_kill(proc_data, 2.0, f"wl-paste --type {target_mime}")
                        if proc_data.returncode == 0 and stdout_data:
                            return stdout_data, target_mime
                text_mimes = ['text/plain', 'text/plain;charset=utf-8', 'UTF8_STRING', 'STRING']
                if any(t in available_types for t in text_mimes):
                    proc_text = await subprocess.create_subprocess_exec(
                        "wl-paste", "--no-newline", # Ensure exact content
                        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                        env=self._get_wl_env()
                    )
                    stdout_text, _ = await self._communicate_or_kill(proc_text, 1.0, "wl-paste --no-newline")
                    if proc_text.returncode == 0:
                        return stdout_text.decode('utf-8', errors='replace'), 'text/plain'

                return None, None

            except Exception as e:
                logger_webrtc_input.warning(f"Error reading Wayland clipboard: {e}")
                return None, None
        monitor = self._ensure_x11_clipboard_monitor()
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
                for mime_type in ['image/png', 'image/jpeg', 'image/bmp', 'image/svg', 'image/webp', 'image/svg+xml']:
                    if mime_type in targets:
                        proc_data = await subprocess.create_subprocess_exec(
                            "xclip", "-selection", "clipboard", "-o", "-t", mime_type,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE
                        )
                        stdout_data, _ = await self._communicate_or_kill(proc_data, 3, f"xclip {mime_type}")
                        if proc_data.returncode == 0 and stdout_data:
                            return stdout_data, mime_type

                # Allow copying of images from file managers
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
        except Exception as e:
            logger_webrtc_input.warning(f"Error reading clipboard with xclip: {e}", exc_info=True)
            return None, None

    async def write_clipboard(self, data, mime_type="text/plain"):
        if not data:
            return True
        input_bytes = data if isinstance(data, bytes) else data.encode('utf-8')
        # Client-supplied content becomes the monitor baseline BEFORE the write: the
        # ownership change fires the monitor immediately, and re-broadcasting what a
        # client just sent is the echo loop that saturates the transport.
        self._clipboard_last_bytes = input_bytes

        env = self._get_wl_env() if self.is_wayland else os.environ.copy()
        if 'LANG' not in env or env['LANG'] == 'C':
            env['LANG'] = 'C.UTF-8'

        if self.is_wayland:
            # Native first: the compositor (pixelflux) owns the selection directly;
            # the wl-copy fork below is the fallback for sessions without the API.
            if self.wayland_input is not None and hasattr(self.wayland_input, 'set_clipboard'):
                try:
                    self.wayland_input.set_clipboard(mime_type, input_bytes)
                    return True
                except Exception as e:
                    logger_webrtc_input.warning(f"native wayland clipboard set failed, using wl-copy: {e}")
            try:
                cmd = ["wl-copy", "--type", mime_type]
                process = await subprocess.create_subprocess_exec(
                    *cmd,
                    stdin=subprocess.PIPE,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    env=env
                )
                if process.stdin:
                    process.stdin.write(input_bytes)
                    await process.stdin.drain()
                    process.stdin.close()
                await self._communicate_or_kill(process, 2.0, f"wl-copy --type {mime_type}")
                if process.returncode == 0:
                    return True
                else:
                    logger_webrtc_input.warning(f"wl-copy failed code: {process.returncode}")
                    return False
            except Exception as e:
                logger_webrtc_input.warning(f"wl-copy exception: {e}")
                return False
        # Native first: own the selection on the monitor's connection; the xclip -i
        # fork below is the fallback (e.g. unsupported mime, monitor unavailable).
        monitor = self._ensure_x11_clipboard_monitor()
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
            if process.stdin:
                process.stdin.write(input_bytes)
                await process.stdin.drain()
                process.stdin.close()
            return_code = await self._wait_or_kill(process, 2.0, f"xclip -i {target_mime}")
            if return_code == 0:
                return True
            else:
                logger_webrtc_input.warning(f"xclip process exited with non-zero code: {return_code}")
                return False
        except asyncio.TimeoutError:
            logger_webrtc_input.warning("Timeout waiting for xclip process to terminate.")
            return False
        except Exception:
            logger_webrtc_input.warning("Error writing to clipboard with xclip", exc_info=True)
            return False
    def _ensure_x11_clipboard_monitor(self):
        """Get-or-create the event-driven X11 monitor; None if unavailable."""
        if self._x11_clipboard_monitor is not None:
            return self._x11_clipboard_monitor
        if self.is_wayland or not X11_LIBS_AVAILABLE:
            return None
        try:
            self._x11_clipboard_monitor = _X11ClipboardMonitor()
            logger_webrtc_input.info("X11 clipboard: XFixes event monitor active (no polling).")
        except Exception as e:
            logger_webrtc_input.info(f"X11 clipboard: XFixes monitor unavailable ({e}); falling back to polling.")
            self._x11_clipboard_monitor = None
        return self._x11_clipboard_monitor

    async def _wait_wayland_clipboard_change(self, timeout):
        """One long-lived `wl-paste --watch` process emits a line per selection
        change; await one line (True) or timeout (False). Falls back to a plain
        sleep (poll cadence) if wl-paste is missing."""
        proc = self._wl_clipboard_watch_proc
        if proc is None or proc.returncode is not None:
            try:
                proc = await subprocess.create_subprocess_exec(
                    "wl-paste", "--watch", "echo", "1",
                    stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                    env=self._get_wl_env())
                self._wl_clipboard_watch_proc = proc
            except Exception:
                self._wl_clipboard_watch_proc = None
                await asyncio.sleep(timeout)
                return True  # no watcher: behave like the old poll
        try:
            line = await asyncio.wait_for(proc.stdout.readline(), timeout)
            if line == b"":  # watcher died; retry next cycle
                self._wl_clipboard_watch_proc = None
                return False
            # Coalesce bursts (wl-paste emits once per offer; apps can re-offer).
            await asyncio.sleep(0.05)
            return True
        except asyncio.TimeoutError:
            return False

    def _arm_wayland_native_clipboard(self):
        """Register the compositor clipboard callback (fork-free watch+read);
        returns the delivery queue, or None when the API is unavailable."""
        if not (self.wayland_input is not None
                and hasattr(self.wayland_input, 'set_clipboard_callback')):
            return None
        try:
            loop = asyncio.get_running_loop()
            queue = asyncio.Queue(maxsize=4)

            def _on_clip(mime, data):
                # Cache for on-demand reads (cr/REQUEST_CLIPBOARD): the compositor
                # already handed us the current selection, no wl-paste fork needed.
                self._wl_native_last = (bytes(data), mime)

                def _put():
                    if queue.full():
                        queue.get_nowait()
                    queue.put_nowait((data, mime))
                loop.call_soon_threadsafe(_put)

            # Kept on self so the monitor loop can re-arm the SAME callback after
            # capture cycles (the compositor drops callbacks on StopCapture).
            self._wl_native_deliver = _on_clip
            self.wayland_input.set_clipboard_callback(_on_clip)
            logger_webrtc_input.info("Wayland clipboard: native compositor callback active (no polling).")
            return queue
        except Exception as e:
            logger_webrtc_input.info(f"Wayland clipboard: native callback unavailable ({e}); using wl-paste watch.")
            return None

    async def start_clipboard(self):
        if self.enable_clipboard not in ["true", "out"]:
            logger_webrtc_input.info("Skipping outbound clipboard service."); return

        # Singleton guard with a grace window: a mode switch stops the old monitor and
        # starts the new one immediately — wait briefly for the old loop to unwind
        # instead of refusing (which left sessions with no outbound clipboard until
        # the user toggled the setting).
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
        x11_monitor = self._ensure_x11_clipboard_monitor()
        wl_native_queue = self._arm_wayland_native_clipboard() if self.is_wayland else None
        wl_native_item = None
        # Prime the change signal so the first pass publishes current content once.
        first_pass = True
        try:
            while self.clipboard_running:
                try:
                    # Event-driven wait (XFixes / compositor callback / wl-paste
                    # --watch); the timeout only re-checks liveness flags. Polling
                    # remains as the last fallback.
                    wl_native_item = None
                    if first_pass:
                        changed = True
                        first_pass = False
                    elif x11_monitor is not None:
                        changed = await x11_monitor.wait_change(2.0)
                    elif wl_native_queue is not None:
                        try:
                            wl_native_item = await asyncio.wait_for(wl_native_queue.get(), 2.0)
                            changed = True
                        except asyncio.TimeoutError:
                            changed = False
                            # The compositor drops its callbacks on every capture stop
                            # (interpreter-finalization safety), and captures cycle with
                            # client connects. Re-arming is an idempotent channel send;
                            # doing it each idle tick keeps the watch alive across cycles.
                            try:
                                self.wayland_input.set_clipboard_callback(
                                    self._wl_native_deliver)
                            except Exception:
                                pass
                    elif self.is_wayland:
                        changed = await self._wait_wayland_clipboard_change(2.0)
                    else:
                        await asyncio.sleep(0.5)
                        changed = True  # poll cadence: read + compare below

                    if not changed:
                        continue
                    if not self._clipboard_has_consumers():
                        # Keep the baseline: reconnecting clients must not trigger a
                        # re-broadcast of unchanged content.
                        continue
                    if getattr(self, 'clipboard_paused', False):
                        continue

                    use_binary = self.enable_binary_clipboard in ["true", "out"]
                    if wl_native_item is not None:
                        # The compositor callback already delivered the bytes.
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
            if self._wl_clipboard_watch_proc is not None:
                try:
                    self._wl_clipboard_watch_proc.kill()
                except ProcessLookupError:
                    pass
                self._wl_clipboard_watch_proc = None
            logger_webrtc_input.info("Clipboard monitor stopped")

    def stop_clipboard(self):
        self.clipboard_running = False
        # Release the dedicated X connection + event thread; a mode switch builds a
        # fresh input handler and must not leak one monitor per transition.
        if self._x11_clipboard_monitor is not None:
            self._x11_clipboard_monitor.close()
            self._x11_clipboard_monitor = None
        logger_webrtc_input.info("Stopping clipboard monitor")


    async def start_cursor_monitor(self):
        if self.is_wayland:
            logger_webrtc_input.info("Wayland mode: Cursor monitor disabled (handled by compositor callback).")
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
            # The X fetch must stay on this thread (python-xlib connections are not
            # thread-safe and the loop also injects input on this display), but the
            # PIL resize + PNG encode is pure CPU: keep it off the event loop.
            cursor_data = await asyncio.to_thread(self.cursor_to_msg, cursor_image)
            self.on_cursor_change(cursor_data)
        except Exception as e:
            logger_webrtc_input.warning("exception from fetching initial cursor image: %s", e)

        while self.cursors_running:
            if self.xdisplay.pending_events() == 0:
                await asyncio.sleep(0.02)
                continue

            event = self.xdisplay.next_event()
            if (event.type, 0) == self.xdisplay.extension_event.DisplayCursorNotify:
                try:
                    cursor_image = self.xdisplay.xfixes_get_cursor_image(screen.root)
                    cursor_data = await asyncio.to_thread(self.cursor_to_msg, cursor_image)
                    self.on_cursor_change(cursor_data)
                except Exception as e:
                    logger_webrtc_input.warning(
                        "exception from fetching cursor image on change: %s", e
                    )
        logger_webrtc_input.info("cursor monitor stopped")

    def stop_cursor_monitor(self):
        logger_webrtc_input.info("stopping cursor monitor")
        self.cursors_running = False

    def get_current_cursor_data(self):
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
            screen = self.xdisplay.screen()
            cursor_image = self.xdisplay.xfixes_get_cursor_image(screen.root)
            return self.cursor_to_msg(cursor_image)
        except Exception as e:
            logger_webrtc_input.warning("exception from fetching current cursor image: %s", e)
            return None

    def _cursor_image_to_pil(self, cursor):
        byte_data = b''.join(p.to_bytes(4, 'little') for p in cursor.cursor_image)
        return Image.frombytes("RGBA", (cursor.width, cursor.height), byte_data, "raw", "BGRA")

    def cursor_to_msg(self, cursor):
        if not cursor or cursor.width == 0 or cursor.height == 0:
            return {
                "curdata": "", "width": 0, "height": 0,
                "hotx": 0, "hoty": 0, "handle": cursor.cursor_serial if cursor else 0,
            }
        im = self._cursor_image_to_pil(cursor)
        bbox = im.getbbox()
        if bbox is None:
            return {
                "curdata": "", "width": 0, "height": 0,
                "hotx": 0, "hoty": 0, "handle": cursor.cursor_serial,
            }
        cropped_im = im.crop(bbox)
        left, upper, right, lower = bbox
        new_hotx = cursor.xhot - left
        new_hoty = cursor.yhot - upper
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
            new_hotx = int(new_hotx * scale_factor)
            new_hoty = int(new_hoty * scale_factor)
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
            "handle": cursor.cursor_serial,
        }

    async def stop_gamepad_servers(self):
        logger_webrtc_input.info("Stopping all gamepad instances.")
        await self.__gamepad_disconnect()

    async def _keyboard_worker(self):
        unicode_buffer = []

        def native_inject():
            # The keymap owner injects any keysym in order (overlay-bound when
            # needed), so text needs no batching; the buffer + wtype/clipboard
            # batch survive only for sessions without the compositor keymap API.
            return bool(self.wayland_input and hasattr(self.wayland_input, 'set_keymap_string'))

        async def flush_buffer():
            if unicode_buffer:
                combined_text = "".join(unicode_buffer)
                unicode_buffer.clear()

                # Buffered text is, by construction, the non-layout-representable keysyms
                # (Latin-1 accents, Euro, Unicode plane) and IME composition strings. Inject
                # it as text — clipboard paste under KWin, wtype under wlroots — never through
                # the keymap, whose overlay swap is a non-op outside the English layout.
                if getattr(self, 'use_clipboard_fallback', False):
                    await self._inject_unicode_via_clipboard(combined_text)
                else:
                    try:
                        cmd = ["wtype", "--", combined_text]
                        proc = await subprocess.create_subprocess_exec(
                            *cmd,
                            stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL,
                            env=self._get_wl_env()
                        )
                        try:
                            await asyncio.wait_for(proc.communicate(), timeout=2.0)
                        except asyncio.TimeoutError:
                            try:
                                proc.kill()
                            except ProcessLookupError:
                                pass
                            await proc.wait()
                            raise
                    except Exception as e:
                        logger_webrtc_input.warning(f"Batched wtype failed: {e}")

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
                        if (is_unicode_fallback and not self.active_modifiers
                                and not native_inject()):
                            unicode_codepoint = keysym & 0x00FFFFFF if (keysym & 0xFF000000) == 0x01000000 else keysym
                            try:
                                char_to_type = chr(unicode_codepoint)
                                unicode_buffer.append(char_to_type)
                                continue
                            except ValueError:
                                pass

                        if keysym == 65288 and unicode_buffer:
                            unicode_buffer.pop()
                            continue

                        await flush_buffer()

                        if keysym in self.MODIFIER_KEYSYMS:
                            self.active_modifiers.add(keysym)

                        await self.send_x11_keypress(keysym, down=True)

                    elif msg_type == "ku":
                        if is_unicode_fallback and not native_inject():
                            # Buffered text was typed atomically; there is no held key.
                            continue

                        if keysym in self.MODIFIER_KEYSYMS:
                            self.active_modifiers.discard(keysym)
                        if keysym in self.atomically_typed_keys:
                            self.atomically_typed_keys.discard(keysym)
                        else:
                            await self.send_x11_keypress(keysym, down=False)

                    elif msg_type == "kr":
                        await flush_buffer()
                        await self.reset_keyboard()

                    elif msg_type == "co_end":
                        if native_inject():
                            # Ordered per-char injection through the compositor channel.
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

    def _reset_multipart_clipboard(self):
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

    async def on_message(self, msg: str, display_id='primary', conn_id=None):
        # A malformed client message must not tear down the transport connection.
        try:
            await self._dispatch_message(msg, display_id, conn_id)
        except (IndexError, ValueError) as e:
            logger_webrtc_input.warning(f"Malformed client message {msg[:64]!r}: {e}")
        except Exception as e:
            logger_webrtc_input.error(f"Error handling client message {msg[:64]!r}: {e}", exc_info=True)

    async def _dispatch_message(self, msg: str, display_id='primary', conn_id=None):
        toks = msg.split(",")
        msg_type = toks[0]

        if msg_type == "pong":
            if self.ping_start is None: logger_webrtc_input.warning("received pong before ping"); return
            self.on_ping_response(float("%.3f" % ((time.time() - self.ping_start) / 2 * 1000)))
        elif msg_type == "kd":
            keysym = int(toks[1])
            # Cap the held-key map against a kd-flood. On a new keysym at the cap,
            # evict the oldest (LRU) rather than the new one: the key is always
            # injected below, so an untracked held key would never get auto-released.
            if keysym in self.pressed_keys:
                self.pressed_keys[keysym] = time.monotonic()
            else:
                if len(self.pressed_keys) >= self.max_pressed_keys:
                    oldest_keysym = min(self.pressed_keys, key=self.pressed_keys.get)
                    self.pressed_keys.pop(oldest_keysym, None)
                self.pressed_keys[keysym] = time.monotonic()
            # A fresh press makes the key live again: drop any reaped-atomic marker
            # so its next 'ku' is honored normally (not swallowed as a stale reap).
            self.reaped_atomic_keys.discard(keysym)
            if self.is_wayland:
                self.keyboard_queue.put_nowait(("kd", keysym))
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
                # Arm X11 server-side auto-repeat for this held key. Only the
                # newest held key repeats, so move it to the end of the insertion-ordered
                # map (pop+insert). Modifiers never repeat. Atomically-typed keys
                # (digits/punctuation typed once via co,end) ARE armed: real keyboards
                # repeat these, and _key_repeat_loop re-dispatches the same atomic type
                # action for them rather than a raw X11 KeyPress.
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
                self.keyboard_queue.put_nowait(("ku", keysym))
            else:
                if keysym in self.MODIFIER_KEYSYMS:
                    self.active_modifiers.discard(keysym)
                if keysym in self.reaped_atomic_keys:
                    # The stale-sweep already reaped this atomically-typed key
                    # (which was never physically held on X11). Swallow the late,
                    # legit 'ku' so we don't inject a spurious keyup for it.
                    self.reaped_atomic_keys.discard(keysym)
                elif keysym in self.atomically_typed_keys:
                    # Atomically-typed key was never physically held on X11, so
                    # there is no matching key-up to inject — just clear tracking.
                    self.atomically_typed_keys.discard(keysym)
                else:
                    await self.send_x11_keypress(keysym, down=False)
        elif msg_type == "kr":
            if self.is_wayland:
                self.keyboard_queue.put_nowait(("kr", None))
            else:
                await self.reset_keyboard()
        elif msg_type == "kh":
            # Heartbeat for held keys: refresh timestamps only (no injection),
            # so it costs nothing but keeps the stale-sweep from releasing them.
            now = time.monotonic()
            # Cap the fan-out at the tracked-key limit: refreshing more keys than we
            # can track is meaningless and a client could otherwise pack one frame
            # with tens of thousands of tokens (unbounded int()+dict work, DoS).
            for tok in toks[1:1 + self.max_pressed_keys]:
                try:
                    keysym = int(tok)
                except ValueError:
                    continue
                # Atomically-typed keys (X11) were never physically held; do not
                # let an ongoing heartbeat pin their lingering pressed_keys entry
                # past the stale window — let the sweep reap it. Safe because atomic
                # (non-alpha printable) keys are single-shot: typed once via co,end on
                # kd and never held, so they have no real heartbeat to preserve.
                if keysym in self.atomically_typed_keys:
                    continue
                if keysym in self.pressed_keys:
                    self.pressed_keys[keysym] = now
        elif msg_type in ["m", "m2"]:
            relative = msg_type == "m2"
            try: x, y, button_mask, scroll_magnitude = [int(i) for i in toks[1:]]
            except: x,y,button_mask,scroll_magnitude = 0,0,self.button_mask,0; relative=False
            try: await self.send_x11_mouse(x, y, button_mask, scroll_magnitude, relative, display_id=display_id)
            except Exception as e: logger_webrtc_input.warning(f"Failed to set mouse cursor: {e}")
        elif msg_type == "p": await self.on_mouse_pointer_visible(bool(int(toks[1])))
        elif msg_type == "vb":
            try:
                # Mbps; fractional values carry sub-Mbps targets. Per-display: the
                # sending page's channel names the display whose stream it tunes.
                bitrate = float(toks[1])
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
            # Deployment policy: when the gamepad add-on is disabled, drop every gamepad
            # message (connect/disconnect/button/axis) server-side so a client can't
            # inject controller input regardless of its own UI state.
            if not settings.gamepad_enabled[0]:
                return
            cmd = toks[1]
            gamepad_idx = int(toks[2])

            if not (0 <= gamepad_idx < self.num_gamepads):
                logger_webrtc_input.error(f"Client message for gamepad index {gamepad_idx} is out of range (0-{self.num_gamepads-1}).")
                return

            # Get the persistent gamepad instance. It should always exist after connect().
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
                
                await self.__gamepad_connect(gamepad_idx, client_name_decoded, client_num_btns, client_num_axes)

            elif cmd == "d": 
                await self.__gamepad_disconnect(gamepad_idx)
            
            elif cmd == "b": 
                button_num = int(toks[3])
                button_val = float(toks[4])
                # Send event to the persistent target_gamepad_instance
                target_gamepad_instance.send_event(button_num, button_val, is_button_event=True)

            elif cmd == "a": 
                axis_num = int(toks[3])
                axis_val = float(toks[4])
                # Send event to the persistent target_gamepad_instance
                target_gamepad_instance.send_event(axis_num, axis_val, is_button_event=False)
            
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
            # Direction gate AND binary gate: honest clients don't send binary while
            # it's off, but the server must enforce its own policy regardless.
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
            # A well-formed chunk is "<type>,<id>,<b64>" (3 tokens). Validate the
            # token count *before* indexing so a malformed (e.g. comma-less) chunk
            # mid-transfer doesn't raise IndexError and leave the multipart state
            # half-open, accumulating until cwe/overflow.
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
            # A well-formed end is "<type>,<id>" (2 tokens). Guard the count
            # before indexing toks[1] so a malformed end doesn't raise mid-state.
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
                    # Awaited in-line: messages on this channel are ordered, so a paste
                    # keystroke right behind the transfer must find the clipboard already
                    # set (write_clipboard's internals are timeout-bounded).
                    if mime_type == "text/plain":
                        text_data = data.decode("utf-8", "ignore")
                        if await self.write_clipboard(text_data):
                            logger_webrtc_input.info(f"Set multi-part clipboard content, length: {len(text_data)}")
                    else:
                        if await self.write_clipboard(data, mime_type=mime_type):
                            logger_webrtc_input.info(f"Set multi-part binary clipboard content ({mime_type}), size: {len(data)} bytes")
                self._reset_multipart_clipboard()
        elif msg_type == "cr":
            if self.enable_clipboard in ["true", "out"]:
                data, mime_type = await self.read_clipboard(use_binary=self.enable_binary_clipboard in ["true", "out"])
                if data:
                    await self.on_clipboard_read(data, mime_type)
                else: logger_webrtc_input.debug("No clipboard content to send on request")
            else: logger_webrtc_input.warning("Rejecting clipboard read: outbound clipboard disabled.")
        elif msg_type == "REQUEST_CLIPBOARD":
            # Client sends REQUEST_CLIPBOARD on Ctrl/Cmd+C and awaits the next
            # clipboard,<b64> / clipboard_binary,... message. Read the server clipboard
            # now and push it via the usual on_clipboard_read path (no new wire format).
            if self.enable_clipboard in ["true", "out"]:
                now = time.monotonic()
                # Per-connection debounce: one client's copy must not suppress another's
                # read. Fall back to display_id when no per-connection id is supplied.
                clip_key = conn_id if conn_id is not None else display_id
                # Evict entries older than the debounce window so the dict can't grow
                # unbounded across reconnecting connections.
                if len(self._last_clipboard_request_ts) > 64:
                    self._last_clipboard_request_ts = {
                        k: ts for k, ts in self._last_clipboard_request_ts.items()
                        if now - ts < self._clipboard_request_debounce
                    }
                # Debounce bursts: one read/fork per copy keystroke would be a fork storm.
                if now - self._last_clipboard_request_ts.get(clip_key, 0.0) < self._clipboard_request_debounce:
                    logger_webrtc_input.debug("Debouncing REQUEST_CLIPBOARD (too frequent).")
                else:
                    self._last_clipboard_request_ts[clip_key] = now
                    use_binary = self.enable_binary_clipboard in ["true", "out"]
                    async def _send_requested_clipboard():
                        # Run read_clipboard as a task so a slow read can't stall the
                        # dispatch loop; reuse the on_clipboard_read send path.
                        try:
                            data, mime_type = await self.read_clipboard(use_binary=use_binary)
                            if data:
                                await self.on_clipboard_read(data, mime_type)
                            else:
                                logger_webrtc_input.debug("No clipboard content to send on REQUEST_CLIPBOARD.")
                        except Exception as e:
                            logger_webrtc_input.warning(f"REQUEST_CLIPBOARD read failed: {e}")
                    self._spawn_task(_send_requested_clipboard())
            else:
                logger_webrtc_input.warning("Rejecting REQUEST_CLIPBOARD: outbound clipboard disabled.")
        elif msg_type == "cb":
            # Same double gate as cbs: the rejection below is only accurate if the
            # binary policy is actually checked here.
            if self.enable_clipboard in ["true", "in"] and self.enable_binary_clipboard in ["true", "in"]:
                try:
                    _, mime_type, b64_data = toks
                    data_bytes = base64.b64decode(b64_data)
                    # In-line so an immediately-following paste keystroke (same ordered
                    # channel) pastes THIS content, not the previous clipboard.
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
                w, h = [int(i) + int(i)%2 for i in res.split("x")]
                # The handler may be an async binding or a sync "disabled" fallback —
                # await only a real coroutine so a skipped resize logs its warning
                # instead of raising 'await None'.
                _r = self.on_resize(f"{w}x{h}", display_id)
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
                home_directory = os.path.expanduser("~")
                try:
                    # Use asyncio subprocess for fire-and-forget execution
                    # stdout and stderr are redirected to DEVNULL to ignore output.
                    process = await subprocess.create_subprocess_shell(
                        command_to_run,
                        stdout=subprocess.DEVNULL,
                        stderr=subprocess.DEVNULL,
                        cwd=home_directory
                    )
                    logger_webrtc_input.info(f"Successfully launched command: '{command_to_run}'")
                except Exception as e:
                    logger_webrtc_input.error(f"Failed to launch command '{command_to_run}': {e}")
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
            except: logger_webrtc_input.error(f"Failed to parse client FPS: {toks}")
        elif msg_type == "_l": 
            try: self.on_client_latency(int(toks[1]))
            except: logger_webrtc_input.error(f"Failed to parse client latency: {toks}")
        elif msg_type in ["_stats_video", "_stats_audio"]: 
            try: await self.on_client_webrtc_stats(msg_type, ",".join(toks[1:]))
            except: logger_webrtc_input.error("Failed to parse WebRTC Statistics")
        elif msg_type == "co" and toks[1] == "end":
            try:
                text_to_type = msg[7:]
                if self.is_wayland:
                    self.keyboard_queue.put_nowait(("co_end", text_to_type))
                elif self._type_text_xtest(text_to_type):
                    # Injected in-process via XTEST (mapped chars with shift synthesis,
                    # unmapped Unicode via the spare-keycode overlay) — no per-char
                    # xdotool fork. Falls through to xdotool below only if that fails.
                    pass
                else:
                    cmd = ["xdotool", "type", text_to_type]
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
                # Settings apply to the display whose channel delivered them (the
                # transport-level id, not a payload field a page could spoof).
                self._spawn_task(self.on_update_settings(settings_json, display_id))
            except Exception as e:
                logger_webrtc_input.error(f"Failed to parse SETTINGS data: {e}")
        elif toks[0] == "SET_NATIVE_CURSOR_RENDERING":
            # WS-protocol alias for the pointer-visibility toggle ("p,N"): in the
            # WebRTC pipeline both map to the same capture_cursor tunable, and the
            # handler no-ops when the value is already set.
            try:
                await self.on_mouse_pointer_visible(toks[1].strip().lower() in ("1", "true"))
            except (IndexError, ValueError) as e:
                logger_webrtc_input.warning(f"Malformed SET_NATIVE_CURSOR_RENDERING message: {msg[:60]}, error: {e}")
        else:
            logger_webrtc_input.info(f"Unknown data channel message: {msg[:100]}")

    def initialize_upload_dir(self):
        if self.upload_dir in ["/sys", "/proc", "/dev"]:
            logger_webrtc_input.info("Can not initialize upload directory at /sys /proc /dev locations")
            return
        if not self.upload_dir:
            logger_webrtc_input.info("Upload dir is empty")
            return

        if self.upload_dir == "~/Desktop":
            # expand the user dir path
            self.upload_dir_path = os.path.expanduser(self.upload_dir)
        else:
            self.upload_dir_path = self.upload_dir

        try:
            os.makedirs(self.upload_dir_path, exist_ok=True)
            logger_webrtc_input.info(f"Upload directory ensured: {self.upload_dir_path}")
        except OSError as e:
            logger_webrtc_input.error(f"Could not create upload directory {self.upload_dir_path}: {e}")
            self.upload_dir_path = None


# MOUSE_POSITION
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

# UINPUT constants if uinput_mouse_socket_path is used
UINPUT_BTN_LEFT = (EV_KEY, BTN_LEFT) 
UINPUT_BTN_MIDDLE = (EV_KEY, BTN_MIDDLE) 
UINPUT_BTN_RIGHT = (EV_KEY, BTN_RIGHT) 
UINPUT_REL_X = (EV_REL, 0x00) # REL_X
UINPUT_REL_Y = (EV_REL, 0x01) # REL_Y
UINPUT_REL_WHEEL = (EV_REL, 0x08) # REL_WHEEL

# X core pointer button numbers used by XTEST fake_input (1=left, 2=middle, 3=right).
XBUTTON_LEFT = 1
XBUTTON_MIDDLE = 2
XBUTTON_RIGHT = 3

MOUSE_BUTTON_MAP = {
    MOUSE_BUTTON_LEFT_ID: {"uinput": UINPUT_BTN_LEFT, "x11": XBUTTON_LEFT},
    MOUSE_BUTTON_MIDDLE_ID: {"uinput": UINPUT_BTN_MIDDLE, "x11": XBUTTON_MIDDLE},
    MOUSE_BUTTON_RIGHT_ID: {"uinput": UINPUT_BTN_RIGHT, "x11": XBUTTON_RIGHT},
}
