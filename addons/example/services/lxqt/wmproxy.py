#!/usr/bin/env python3
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
"""Wayland proxy giving the native LXQt session its window verbs on sway.

A native taskbar controls windows through wlr-foreign-toplevel-management,
and sway's i3 model silently drops the set_maximized and set_minimized
requests — they travel client-to-compositor on a private socket, so nothing
beside the compositor ever sees them. The session therefore connects through
this proxy: every byte and file descriptor is forwarded verbatim in both
directions (buffers cross as descriptors, so no pixel data ever flows here),
while a mirror of the streams tracks just enough state to recognise the
dropped verbs and perform them through sway IPC instead:

  - set_maximized resizes the floating container to its workspace rectangle
    (sway has already subtracted layer-shell exclusive zones from it) and
    remembers the old geometry; on a window already that size it restores,
    since without compositor-side state the taskbar keeps offering Maximize;
  - unset_maximized restores the remembered geometry;
  - set_minimized hides the container in the scratchpad;
  - unset_minimized and activate bring a window this proxy minimized back
    out before sway's own activation handling focuses it.

Requests are still forwarded after translation — sway ignores the ones it
drops, and every other request keeps its native handling. Toplevels are
matched to sway containers by app_id and title from the mirrored handle
events."""
import os
import select
import socket
import struct
import sys
import time

FT_MANAGER = b"zwlr_foreign_toplevel_manager_v1"
REQ_SET_MAXIMIZED = 0
REQ_UNSET_MAXIMIZED = 1
REQ_SET_MINIMIZED = 2
REQ_UNSET_MINIMIZED = 3
REQ_ACTIVATE = 4
REQ_DESTROY = 7
EV_MANAGER_TOPLEVEL = 0
EV_HANDLE_TITLE = 0
EV_HANDLE_APP_ID = 1
EV_HANDLE_CLOSED = 6

WL_DISPLAY = 1
REQ_GET_REGISTRY = 1
REQ_REGISTRY_BIND = 0

I3_RUN_COMMAND = 0
I3_GET_TREE = 4
I3_GET_VERSION = 7


def log(msg):
    print("[wmproxy] " + msg, file=sys.stderr, flush=True)


class SwayIPC:
    """Minimal i3 IPC client on a persistent socket."""

    def __init__(self):
        self.sock = None

    def _connect(self):
        runtime = os.environ.get("XDG_RUNTIME_DIR", "/tmp")
        candidates = [os.environ["SWAYSOCK"]] if os.environ.get("SWAYSOCK") else []
        for name in sorted(os.listdir(runtime)):
            if name.startswith("sway-ipc.") and name.endswith(".sock"):
                candidates.append(os.path.join(runtime, name))
        for path in candidates:
            try:
                s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                s.settimeout(5)
                s.connect(path)
                self.sock = s
                self._send(I3_GET_VERSION, b"")
                self._recv()
                return
            except OSError:
                continue
        raise RuntimeError("no responding sway IPC socket")

    def _send(self, mtype, payload):
        self.sock.sendall(b"i3-ipc" + struct.pack("<II", len(payload), mtype) + payload)

    def _recv(self):
        header = b""
        while len(header) < 14:
            chunk = self.sock.recv(14 - len(header))
            if not chunk:
                raise OSError("sway IPC closed")
            header += chunk
        length = struct.unpack("<I", header[6:10])[0]
        payload = b""
        while len(payload) < length:
            chunk = self.sock.recv(length - len(payload))
            if not chunk:
                raise OSError("sway IPC closed")
            payload += chunk
        return payload

    def call(self, mtype, payload=b""):
        for attempt in (1, 2):
            try:
                if self.sock is None:
                    self._connect()
                self._send(mtype, payload)
                return self._recv()
            except OSError:
                self.sock = None
                if attempt == 2:
                    raise
        return None

    def command(self, cmd):
        return self.call(I3_RUN_COMMAND, cmd.encode())

    def tree(self):
        import json
        return json.loads(self.call(I3_GET_TREE).decode())


def iter_views(node, acc):
    for child in node.get("nodes", []) + node.get("floating_nodes", []):
        iter_views(child, acc)
    if node.get("pid") or node.get("app_id") or node.get("window_properties"):
        acc.append(node)


def workspace_of(tree, con_id):
    def walk(node, workspace):
        if node.get("type") == "workspace":
            workspace = node
        if node.get("id") == con_id:
            return workspace
        for child in node.get("nodes", []) + node.get("floating_nodes", []):
            found = walk(child, workspace)
            if found is not None:
                return found
        return None
    return walk(tree, None)


class Verbs:
    """Translates the dropped foreign-toplevel verbs into sway IPC."""

    def __init__(self):
        self.sway = SwayIPC()
        # con_id -> (x, y, w, h) to restore after maximize
        self.restore = {}
        # con_ids this proxy sent to the scratchpad
        self.minimized = set()

    def find_con(self, identity):
        app_id, title = identity.get("app_id"), identity.get("title")
        views = []
        iter_views(self.sway.tree(), views)
        exact = []
        for view in views:
            props = view.get("window_properties") or {}
            candidate_app = view.get("app_id") or props.get("class") or ""
            if app_id and candidate_app != app_id:
                continue
            exact.append(view)
        if len(exact) > 1 and title:
            titled = [v for v in exact if (v.get("name") or "") == title]
            if titled:
                exact = titled
        if not exact:
            return None
        if len(exact) > 1:
            focused = [v for v in exact if v.get("focused")]
            exact = focused or exact
        return exact[0]

    def maximize(self, identity):
        view = self.find_con(identity)
        if view is None:
            return
        con_id, rect = view["id"], view["rect"]
        workspace = workspace_of(self.sway.tree(), con_id)
        if workspace is None:
            return
        area = workspace["rect"]
        # The compositor clamps the commanded size to bars and exclusive
        # zones, so "already maximized" is proportional rather than exact —
        # and a rect that large must never replace the saved restore size.
        large = (rect["width"] * 10 >= area["width"] * 9
                 and rect["height"] * 10 >= area["height"] * 9)
        if large:
            if con_id in self.restore:
                self.unmaximize(identity)
            return
        self.restore[con_id] = (rect["x"], rect["y"], rect["width"], rect["height"])
        self.sway.command(
            "[con_id=%d] resize set %d px %d px, move position %d px %d px"
            % (con_id, area["width"], area["height"], area["x"], area["y"]))
        log("maximized %s to %dx%d" % (identity.get("app_id"), area["width"], area["height"]))

    def unmaximize(self, identity):
        view = self.find_con(identity)
        if view is None:
            return
        saved = self.restore.pop(view["id"], None)
        if not saved:
            return
        self.sway.command(
            "[con_id=%d] resize set %d px %d px, move position %d px %d px"
            % (view["id"], saved[2], saved[3], saved[0], saved[1]))
        log("restored %s to %dx%d" % (identity.get("app_id"), saved[2], saved[3]))

    def minimize(self, identity):
        view = self.find_con(identity)
        if view is None:
            return
        self.minimized.add(view["id"])
        self.sway.command("[con_id=%d] move scratchpad" % view["id"])
        log("minimized %s" % identity.get("app_id"))

    def unminimize(self, identity):
        view = self.find_con(identity)
        if view is None or view["id"] not in self.minimized:
            return
        self.minimized.discard(view["id"])
        self.sway.command("[con_id=%d] scratchpad show, focus" % view["id"])
        log("brought %s back" % identity.get("app_id"))


class Mirror:
    """Frame-aligns one connection's two streams and tracks the objects of
    the foreign-toplevel protocol; everything else passes unexamined."""

    def __init__(self, verbs):
        self.verbs = verbs
        self.buf = {True: b"", False: b""}  # keyed by client_to_server
        self.registries = set()
        self.managers = set()
        self.handles = {}  # object id -> {"app_id": ..., "title": ...}

    @staticmethod
    def wl_string(payload, offset):
        (length,) = struct.unpack_from("<I", payload, offset)
        raw = payload[offset + 4:offset + 4 + length]
        offset += 4 + ((length + 3) & ~3)
        return raw.rstrip(b"\0"), offset

    def feed(self, client_to_server, data):
        self.buf[client_to_server] += data
        buf = self.buf[client_to_server]
        pos = 0
        while len(buf) - pos >= 8:
            obj, size_op = struct.unpack_from("<II", buf, pos)
            size, opcode = size_op >> 16, size_op & 0xFFFF
            if size < 8 or len(buf) - pos < size:
                break
            try:
                self.message(client_to_server, obj, opcode, buf[pos + 8:pos + size])
            except Exception as exc:
                log("mirror parse skipped a message: %s" % exc)
            pos += size
        self.buf[client_to_server] = buf[pos:]

    def message(self, client_to_server, obj, opcode, payload):
        if client_to_server:
            if obj == WL_DISPLAY and opcode == REQ_GET_REGISTRY:
                self.registries.add(struct.unpack_from("<I", payload, 0)[0])
            elif obj in self.registries and opcode == REQ_REGISTRY_BIND:
                interface, offset = self.wl_string(payload, 4)
                if interface == FT_MANAGER:
                    (new_id,) = struct.unpack_from("<I", payload, offset + 4)
                    self.managers.add(new_id)
                    log("taskbar bound the foreign-toplevel manager")
            elif obj in self.handles:
                identity = self.handles[obj]
                if opcode == REQ_SET_MAXIMIZED:
                    self.verbs.maximize(identity)
                elif opcode == REQ_UNSET_MAXIMIZED:
                    self.verbs.unmaximize(identity)
                elif opcode == REQ_SET_MINIMIZED:
                    self.verbs.minimize(identity)
                elif opcode in (REQ_UNSET_MINIMIZED, REQ_ACTIVATE):
                    self.verbs.unminimize(identity)
                elif opcode == REQ_DESTROY:
                    self.handles.pop(obj, None)
        else:
            if obj in self.managers and opcode == EV_MANAGER_TOPLEVEL:
                self.handles[struct.unpack_from("<I", payload, 0)[0]] = {}
            elif obj in self.handles:
                if opcode == EV_HANDLE_TITLE:
                    self.handles[obj]["title"] = self.wl_string(payload, 0)[0].decode(
                        "utf-8", "replace")
                elif opcode == EV_HANDLE_APP_ID:
                    self.handles[obj]["app_id"] = self.wl_string(payload, 0)[0].decode(
                        "utf-8", "replace")
                elif opcode == EV_HANDLE_CLOSED:
                    self.handles.pop(obj, None)


MAX_FDS = 28


class Pump:
    """One proxied connection: two sockets forwarded verbatim, mirrored."""

    def __init__(self, client, upstream, verbs):
        self.client = client
        self.upstream = upstream
        self.mirror = Mirror(verbs)
        # per destination socket: list of [data, fds] not yet fully sent
        self.pending = {client: [], upstream: []}
        for sock in (client, upstream):
            sock.setblocking(False)

    def sockets(self):
        return self.client, self.upstream

    def wants_write(self, sock):
        return bool(self.pending[sock])

    def peer(self, sock):
        return self.upstream if sock is self.client else self.client

    def pull(self, sock):
        """Read one chunk and queue it, with its descriptors, for the peer."""
        data, ancdata, flags, _addr = sock.recvmsg(
            65536, socket.CMSG_SPACE(MAX_FDS * 4))
        if flags & socket.MSG_CTRUNC:
            raise OSError("descriptor batch larger than the proxy accepts")
        fds = []
        for level, ctype, cdata in ancdata:
            if level == socket.SOL_SOCKET and ctype == socket.SCM_RIGHTS:
                fds += list(struct.unpack("%di" % (len(cdata) // 4), cdata))
        if not data and not fds:
            raise OSError("connection closed")
        self.mirror.feed(sock is self.client, data)
        self.pending[self.peer(sock)].append([data, fds])

    def push(self, sock):
        """Write queued chunks; descriptors ride only the first bytes sent."""
        queue = self.pending[sock]
        while queue:
            data, fds = queue[0]
            try:
                if fds:
                    sent = sock.sendmsg([data], [(socket.SOL_SOCKET, socket.SCM_RIGHTS,
                                                  struct.pack("%di" % len(fds), *fds))])
                else:
                    sent = sock.send(data)
            except BlockingIOError:
                return
            if fds:
                for fd in fds:
                    os.close(fd)
                queue[0][1] = []
            if sent < len(data):
                queue[0][0] = data[sent:]
                return
            queue.pop(0)

    def close(self):
        for _data, fds in self.pending[self.client] + self.pending[self.upstream]:
            for fd in fds:
                os.close(fd)
        for sock in (self.client, self.upstream):
            try:
                sock.close()
            except OSError:
                pass


def serve():
    runtime = os.environ.get("XDG_RUNTIME_DIR", "/tmp")
    upstream_name = os.environ.get("WAYLAND_DISPLAY")
    if not upstream_name:
        log("no WAYLAND_DISPLAY to proxy")
        return 1
    upstream_path = (upstream_name if upstream_name.startswith("/")
                     else os.path.join(runtime, upstream_name))
    listen_path = os.path.join(runtime, "wayland-wm")
    try:
        os.unlink(listen_path)
    except OSError:
        pass
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(listen_path)
    listener.listen(8)
    verbs = Verbs()
    log("proxying %s as wayland-wm" % upstream_name)

    pumps = []
    while True:
        readers = [listener]
        writers = []
        by_sock = {}
        for pump in pumps:
            for sock in pump.sockets():
                by_sock[sock] = pump
                readers.append(sock)
                if pump.wants_write(sock):
                    writers.append(sock)
        ready_r, ready_w, _ = select.select(readers, writers, [], 30)
        for sock in ready_w:
            pump = by_sock.get(sock)
            if pump is None:
                continue
            try:
                pump.push(sock)
            except OSError:
                pump.close()
                if pump in pumps:
                    pumps.remove(pump)
        for sock in ready_r:
            if sock is listener:
                try:
                    client, _ = listener.accept()
                    upstream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                    upstream.connect(upstream_path)
                    pumps.append(Pump(client, upstream, verbs))
                except OSError as exc:
                    log("accept failed: %s" % exc)
                continue
            pump = by_sock.get(sock)
            if pump is None or pump not in pumps:
                continue
            try:
                pump.pull(sock)
                pump.push(pump.peer(sock))
            except OSError:
                pump.close()
                pumps.remove(pump)


def main():
    while True:
        try:
            return serve()
        except RuntimeError as exc:
            log("waiting for the compositor: %s" % exc)
            time.sleep(3)
        except Exception as exc:
            log("restarting: %s" % exc)
            time.sleep(3)


if __name__ == "__main__":
    sys.exit(main())
