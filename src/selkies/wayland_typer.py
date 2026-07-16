# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Unicode text injection for Wayland compositors via zwp_virtual_keyboard_v1.

Speaks the Wayland wire protocol directly over the session's unix socket
(stdlib only, natively async): bind zwp_virtual_keyboard_manager_v1, upload an
xkb keymap that maps the needed codepoints onto overlay keycodes above the
standard layout, then press/release those keys in order. wlroots-style
compositors and KWin implement the protocol; Weston does not, which raises
WaylandVirtualKeyboardUnavailable so callers can fall back.

The socket is driven through the running asyncio loop (loop.sock_* plus
add_writer for the one keymap fd-passing send), so every roundtrip and every
inter-key pacing delay is an await that yields to the event loop rather than
blocking a thread.
"""

import asyncio
import os
import socket
import struct
import tempfile

# Object 1 is wl_display; client-allocated ids count up from 2.
_WL_DISPLAY_ID = 1
_OP_DISPLAY_SYNC = 0
_OP_DISPLAY_GET_REGISTRY = 1
_EV_DISPLAY_ERROR = 0
_OP_REGISTRY_BIND = 0
_EV_REGISTRY_GLOBAL = 0
_EV_CALLBACK_DONE = 0
_OP_MANAGER_CREATE_VIRTUAL_KEYBOARD = 0
_OP_KEYBOARD_KEYMAP = 0
_OP_KEYBOARD_KEY = 1
_OP_KEYBOARD_DESTROY = 3

_KEYMAP_FORMAT_XKB_V1 = 1
_KEY_STATE_RELEASED = 0
_KEY_STATE_PRESSED = 1

# wl_seat versions above 7 are untested by the reference tooling; events from
# any bound version are skipped generically, so capping is only conservatism.
_MAX_SEAT_VERSION = 7

# Overlay keys start above the pc+us block so the uploaded keymap can include
# the standard layout intact (a virtual keyboard's keymap can become the seat
# keymap, so it must keep ordinary typing usable). xkb v1 keymaps cap keycodes
# at 255, which bounds one keymap to _MAX_OVERLAY_SLOTS distinct symbols;
# longer texts flush and start a fresh keymap.
_WAYLAND_KEYCODE_OFFSET = 142
_XKB_KEYCODE_OFFSET = _WAYLAND_KEYCODE_OFFSET + 8
_MAX_OVERLAY_SLOTS = 256 - _XKB_KEYCODE_OFFSET

# Control characters must type as their editing keysyms: U#### unicode names
# only resolve to text keysyms, and apps expect Return rather than Linefeed.
_KEYSYM_NAME_REMAP = {
    0x09: "Tab",
    0x0A: "Return",
    0x0D: "Return",
    0x1B: "Escape",
}


def _keysym_name(codepoint):
    """xkb keysym name for a codepoint, or None when untypeable. U#### names
    resolve through xkbcommon for every printable codepoint, so no client-side
    keysym table is needed."""
    name = _KEYSYM_NAME_REMAP.get(codepoint)
    if name is not None:
        return name
    if codepoint < 0x20 or codepoint == 0x7F or 0x80 <= codepoint <= 0x9F:
        return None
    if 0xD800 <= codepoint <= 0xDFFF or codepoint > 0x10FFFF:
        return None
    return "U%04X" % codepoint


class WaylandProtocolError(RuntimeError):
    pass


class WaylandVirtualKeyboardUnavailable(WaylandProtocolError):
    """The compositor does not offer zwp_virtual_keyboard_manager_v1."""


class WaylandVirtualKeyboard:
    """One virtual-keyboard client connection, driven on the asyncio loop.

    Build with `await WaylandVirtualKeyboard.connect(...)`. The instance may be
    kept and reused across type_text() calls: the overlay keymap accumulates,
    so repeated characters skip the keymap re-upload. Not safe for concurrent
    callers on one instance -- serialize with a lock; every method awaits and
    yields the loop between sends.
    """

    def __init__(self, display=None, runtime_dir=None,
                 io_timeout=5.0, key_hold_s=0.002, keymap_settle_s=0.010):
        self._sock = None
        self._buf = bytearray()
        self._next_id = 2
        self._registry_id = 0
        self._seat_id = 0
        self._manager_id = 0
        self._keyboard_id = 0
        self._io_timeout = io_timeout
        # Pacing mirrors wtype: hold each key ~2ms and let clients pick up a
        # fresh keymap for ~10ms before keys that depend on it arrive. As
        # awaits these yield the loop rather than stalling a thread.
        self._key_hold_s = key_hold_s
        self._keymap_settle_s = keymap_settle_s
        self._entries = []
        self._slot_by_name = {}
        self._keymap_dirty = False
        self._path = self._socket_path(display, runtime_dir)

    @classmethod
    async def connect(cls, display=None, runtime_dir=None,
                      connect_timeout=2.0, io_timeout=5.0,
                      key_hold_s=0.002, keymap_settle_s=0.010):
        self = cls(display=display, runtime_dir=runtime_dir,
                   io_timeout=io_timeout, key_hold_s=key_hold_s,
                   keymap_settle_s=keymap_settle_s)
        loop = asyncio.get_running_loop()
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.setblocking(False)
        try:
            await asyncio.wait_for(
                loop.sock_connect(sock, self._path), connect_timeout)
        except (OSError, asyncio.TimeoutError) as e:
            sock.close()
            raise WaylandProtocolError(
                "cannot connect to Wayland socket %s: %s" % (self._path, e))
        self._sock = sock
        try:
            await self._setup()
        except BaseException:
            self.close()
            raise
        return self

    @staticmethod
    def _socket_path(display, runtime_dir):
        display = display or os.environ.get("WAYLAND_DISPLAY") or "wayland-0"
        if os.path.isabs(display):
            return display
        runtime_dir = runtime_dir or os.environ.get("XDG_RUNTIME_DIR")
        if not runtime_dir:
            raise WaylandProtocolError(
                "XDG_RUNTIME_DIR is not set; cannot locate Wayland socket")
        return os.path.join(runtime_dir, display)

    # -- wire helpers ------------------------------------------------------

    def _new_id(self):
        nid = self._next_id
        self._next_id += 1
        return nid

    @staticmethod
    def _string_arg(s):
        # Wire strings: uint32 length including the NUL, bytes, pad to 32 bits.
        b = s.encode("utf-8") + b"\0"
        return struct.pack("=I", len(b)) + b + b"\0" * (-len(b) % 4)

    async def _send(self, obj_id, opcode, payload=b""):
        size = 8 + len(payload)
        msg = struct.pack("=II", obj_id, (size << 16) | opcode) + payload
        await asyncio.get_running_loop().sock_sendall(self._sock, msg)

    async def _send_keymap_fd(self, fd, size):
        # zwp_virtual_keyboard_v1.keymap: the payload is a fixed 16 bytes
        # (header + format + size) and the mapping rides as an SCM_RIGHTS fd,
        # so the inline write is atomic and needs no partial-send loop.
        payload = struct.pack("=II", _KEYMAP_FORMAT_XKB_V1, size)
        msg = struct.pack("=II", self._keyboard_id,
                          ((8 + len(payload)) << 16) | _OP_KEYBOARD_KEYMAP) + payload
        loop = asyncio.get_running_loop()
        anc = [(socket.SOL_SOCKET, socket.SCM_RIGHTS, struct.pack("=i", fd))]
        while True:
            try:
                self._sock.sendmsg([msg], anc)
                return
            except (BlockingIOError, InterruptedError):
                waiter = loop.create_future()
                loop.add_writer(self._sock.fileno(),
                                lambda f=waiter: f.done() or f.set_result(None))
                try:
                    await waiter
                finally:
                    loop.remove_writer(self._sock.fileno())

    async def _recv_into_buf(self):
        # No interface bound here (wl_display/registry/seat/manager/keyboard)
        # emits an fd-carrying event, so a plain recv suffices -- no ancillary
        # handling on the read path.
        data = await asyncio.get_running_loop().sock_recv(self._sock, 65536)
        if not data:
            raise WaylandProtocolError("Wayland connection closed by compositor")
        self._buf += data

    async def _next_message(self):
        while len(self._buf) < 8:
            await self._recv_into_buf()
        obj_id, word = struct.unpack_from("=II", self._buf, 0)
        size = word >> 16
        opcode = word & 0xFFFF
        if size < 8:
            raise WaylandProtocolError("malformed Wayland message header")
        while len(self._buf) < size:
            await self._recv_into_buf()
        payload = bytes(self._buf[8:size])
        del self._buf[:size]
        return obj_id, opcode, payload

    @staticmethod
    def _parse_string(payload, offset):
        (length,) = struct.unpack_from("=I", payload, offset)
        offset += 4
        value = ""
        if length:
            value = payload[offset:offset + length - 1].decode("utf-8", "replace")
        offset += (length + 3) & ~3
        return value, offset

    def _handle_event(self, obj_id, opcode, payload):
        # Only wl_display.error carries state this client must act on; every
        # other unrequested event (delete_id, seat capabilities/name, ...) is
        # skippable because message boundaries are explicit.
        if obj_id == _WL_DISPLAY_ID and opcode == _EV_DISPLAY_ERROR:
            err_obj, code = struct.unpack_from("=II", payload, 0)
            message, _ = self._parse_string(payload, 8)
            raise WaylandProtocolError(
                "compositor protocol error on object %d (code %d): %s"
                % (err_obj, code, message))

    async def _roundtrip(self, registry_globals=None):
        async def _inner():
            callback_id = self._new_id()
            await self._send(_WL_DISPLAY_ID, _OP_DISPLAY_SYNC,
                             struct.pack("=I", callback_id))
            while True:
                obj_id, opcode, payload = await self._next_message()
                if obj_id == callback_id and opcode == _EV_CALLBACK_DONE:
                    return
                if (registry_globals is not None
                        and obj_id == self._registry_id
                        and opcode == _EV_REGISTRY_GLOBAL):
                    (name,) = struct.unpack_from("=I", payload, 0)
                    interface, offset = self._parse_string(payload, 4)
                    (version,) = struct.unpack_from("=I", payload, offset)
                    registry_globals.append((name, interface, version))
                    continue
                self._handle_event(obj_id, opcode, payload)
        await asyncio.wait_for(_inner(), self._io_timeout)

    # -- session setup -----------------------------------------------------

    async def _setup(self):
        self._registry_id = self._new_id()
        await self._send(_WL_DISPLAY_ID, _OP_DISPLAY_GET_REGISTRY,
                         struct.pack("=I", self._registry_id))
        registry_globals = []
        await self._roundtrip(registry_globals=registry_globals)

        seat = None
        manager = None
        for name, interface, version in registry_globals:
            if interface == "wl_seat" and seat is None:
                seat = (name, version)
            elif interface == "zwp_virtual_keyboard_manager_v1":
                manager = (name, version)
        if manager is None:
            raise WaylandVirtualKeyboardUnavailable(
                "compositor does not advertise zwp_virtual_keyboard_manager_v1")
        if seat is None:
            raise WaylandProtocolError("compositor advertises no wl_seat")

        self._seat_id = self._new_id()
        await self._send(self._registry_id, _OP_REGISTRY_BIND,
                         struct.pack("=I", seat[0])
                         + self._string_arg("wl_seat")
                         + struct.pack("=II", min(seat[1], _MAX_SEAT_VERSION),
                                       self._seat_id))
        self._manager_id = self._new_id()
        await self._send(self._registry_id, _OP_REGISTRY_BIND,
                         struct.pack("=I", manager[0])
                         + self._string_arg("zwp_virtual_keyboard_manager_v1")
                         + struct.pack("=II", 1, self._manager_id))
        self._keyboard_id = self._new_id()
        await self._send(self._manager_id, _OP_MANAGER_CREATE_VIRTUAL_KEYBOARD,
                         struct.pack("=II", self._seat_id, self._keyboard_id))
        # Surfaces zwp_virtual_keyboard_manager_v1's "unauthorized" error (or
        # any bad bind) before the first keymap upload.
        await self._roundtrip()
        # The protocol forbids key events on a keymap-less keyboard, and a
        # keymap-less device also degrades compositor seat handling (focus
        # enter has no mapping to send), so the base layout goes up at once;
        # overlay entries re-upload on demand.
        await self._upload_keymap()

    # -- keymap ------------------------------------------------------------

    @staticmethod
    def _build_keymap_text(entries):
        lines = [
            "xkb_keymap {",
            'xkb_keycodes "(unnamed)" {',
            'include "evdev+aliases(qwerty)"',
            "minimum = 8;",
            "maximum = 255;",
        ]
        for i in range(len(entries)):
            lines.append("<K%d> = %d;" % (i + 1, i + _XKB_KEYCODE_OFFSET))
        lines.append("};")
        lines.append('xkb_types "(unnamed)" { include "complete" };')
        lines.append('xkb_compatibility "(unnamed)" { include "complete" };')
        lines.append('xkb_symbols "(unnamed)" {')
        lines.append('include "pc+us"')
        for i, name in enumerate(entries):
            lines.append("override key <K%d> {[%s]};" % (i + 1, name))
        lines.append("};")
        lines.append("};")
        return "\n".join(lines) + "\n"

    @staticmethod
    def _keymap_fd(data):
        try:
            fd = os.memfd_create("selkies-vk-keymap")
        except (AttributeError, OSError):
            fd, path = tempfile.mkstemp(prefix="selkies-vk-keymap")
            os.unlink(path)
        try:
            written = 0
            while written < len(data):
                written += os.write(fd, data[written:])
        except OSError:
            os.close(fd)
            raise
        return fd

    async def _upload_keymap(self):
        # Size includes the terminating NUL: compositors hand the mapping to
        # xkbcommon as a NUL-terminated string.
        data = self._build_keymap_text(self._entries).encode("utf-8") + b"\0"
        fd = self._keymap_fd(data)
        try:
            await self._send_keymap_fd(fd, len(data))
        finally:
            os.close(fd)
        await self._roundtrip()
        if self._keymap_settle_s:
            await asyncio.sleep(self._keymap_settle_s)
        self._keymap_dirty = False

    # -- typing ------------------------------------------------------------

    async def _send_key(self, keycode, pressed):
        await self._send(self._keyboard_id, _OP_KEYBOARD_KEY,
                         struct.pack("=III", 0, keycode,
                                     _KEY_STATE_PRESSED if pressed
                                     else _KEY_STATE_RELEASED))
        if self._key_hold_s:
            await asyncio.sleep(self._key_hold_s)

    async def _flush(self, slots):
        if self._keymap_dirty and self._entries:
            await self._upload_keymap()
        for slot in slots:
            keycode = slot + _WAYLAND_KEYCODE_OFFSET
            await self._send_key(keycode, True)
            await self._send_key(keycode, False)
        if slots:
            # One roundtrip per batch confirms the compositor drained the key
            # events (and any events it sent back) before the caller proceeds.
            # A per-key roundtrip -- as the C reference does -- is unnecessary
            # on an ordered stream and would burn a wait_for per keystroke.
            await self._roundtrip()
        return len(slots)

    async def type_text(self, text):
        """Type `text` in order; codepoints with no keysym are skipped.
        Returns the number of key taps sent."""
        if self._sock is None:
            raise WaylandProtocolError("virtual keyboard is closed")
        typed = 0
        pending = []
        for ch in text:
            name = _keysym_name(ord(ch))
            if name is None:
                continue
            slot = self._slot_by_name.get(name)
            if slot is None:
                if len(self._entries) >= _MAX_OVERLAY_SLOTS:
                    typed += await self._flush(pending)
                    pending = []
                    self._entries = []
                    self._slot_by_name = {}
                slot = len(self._entries)
                self._entries.append(name)
                self._slot_by_name[name] = slot
                self._keymap_dirty = True
            pending.append(slot)
        typed += await self._flush(pending)
        return typed

    def close(self):
        sock = self._sock
        if sock is None:
            return
        self._sock = None
        if self._keyboard_id:
            try:
                # Destructor request; the compositor also reclaims the object
                # on disconnect, so this is best-effort on a nonblocking socket.
                sock.send(struct.pack(
                    "=II", self._keyboard_id, (8 << 16) | _OP_KEYBOARD_DESTROY))
            except OSError:
                pass
        try:
            sock.close()
        except OSError:
            pass
