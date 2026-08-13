#!/usr/bin/env python3
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
"""EWMH bridge for the X11 LXQt session on the nested sway compositor.

sway manages the session's windows but speaks the i3 model, which has no
maximize or iconify verbs: its X window manager mirrors requested
_NET_WM_STATE atoms without touching geometry, drops WM_CHANGE_STATE
iconify requests, ignores configure requests from other X11 clients, and
never stamps _NET_WM_ALLOWED_ACTIONS — so taskbar menus offer no Maximize
or Minimize at all. A floating window whose output is unplugged is also
left on the evacuated workspace, which no output shows, leaving a taskbar
entry whose window can never be brought back.

This bridge supplies those desktop semantics through sway IPC while
speaking EWMH on the X side, so the panel's taskbar and ordinary X11
applications behave as they do under Openbox on the X11 backend:

  - new windows are stamped with _NET_WM_ALLOWED_ACTIONS and cascaded when
    sway would stack them all dead-centre;
  - _NET_WM_STATE maximize requests resize the floating container to the
    output's work area (honouring the panel strut) and restore the saved
    geometry on unmaximize;
  - WM_CHANGE_STATE iconify hides the window in the scratchpad and marks
    it _NET_WM_STATE_HIDDEN so the taskbar draws it minimized;
  - _NET_ACTIVE_WINDOW activation focuses the window, pulling it back out
    of the scratchpad first when it was minimized;
  - windows stranded on a workspace no output shows are moved to the
    workspace visible on the nearest output.

The vendored Xlib inside selkies is used when available so the bridge has
no dependencies beyond the session's own Python.
"""
import glob
import json
import os
import select
import subprocess
import sys
import time

try:
    from selkies.Xlib import X, display as xdisplay
except ImportError:
    from Xlib import X, display as xdisplay


def find_swaysock():
    runtime = os.environ.get("XDG_RUNTIME_DIR", "/tmp")
    for sock in sorted(glob.glob(os.path.join(runtime, "sway-ipc.*.sock"))):
        try:
            subprocess.run(["swaymsg", "-t", "get_version"],
                           env={**os.environ, "SWAYSOCK": sock},
                           capture_output=True, timeout=5, check=True)
            return sock
        except Exception:
            continue
    return None


class Bridge:
    ACTION_NAMES = ("_NET_WM_ACTION_MOVE", "_NET_WM_ACTION_RESIZE",
                    "_NET_WM_ACTION_MINIMIZE", "_NET_WM_ACTION_MAXIMIZE_HORZ",
                    "_NET_WM_ACTION_MAXIMIZE_VERT", "_NET_WM_ACTION_FULLSCREEN",
                    "_NET_WM_ACTION_CLOSE")

    def __init__(self, swaysock):
        self.env = {**os.environ, "SWAYSOCK": swaysock}
        self.dpy = xdisplay.Display()
        self.root = self.dpy.screen().root
        atom = self.dpy.intern_atom
        self.a_state = atom("_NET_WM_STATE")
        self.a_max_v = atom("_NET_WM_STATE_MAXIMIZED_VERT")
        self.a_max_h = atom("_NET_WM_STATE_MAXIMIZED_HORZ")
        self.a_hidden = atom("_NET_WM_STATE_HIDDEN")
        self.a_change_state = atom("WM_CHANGE_STATE")
        self.a_active = atom("_NET_ACTIVE_WINDOW")
        self.a_allowed = atom("_NET_WM_ALLOWED_ACTIONS")
        self.a_type = atom("_NET_WM_WINDOW_TYPE")
        self.a_type_normal = atom("_NET_WM_WINDOW_TYPE_NORMAL")
        self.a_type_dock = atom("_NET_WM_WINDOW_TYPE_DOCK")
        self.a_type_desktop = atom("_NET_WM_WINDOW_TYPE_DESKTOP")
        self.a_strut = atom("_NET_WM_STRUT_PARTIAL")
        self.a_actions = [atom(n) for n in self.ACTION_NAMES]
        self.root.change_attributes(event_mask=X.SubstructureNotifyMask)
        self.dpy.flush()
        # xid -> {"restore": (x, y, w, h) | None, "min": bool}
        self.state = {}
        self.cascade = 0

    # -- sway IPC ---------------------------------------------------------
    def sway(self, *cmd):
        return subprocess.run(["swaymsg", *cmd], env=self.env,
                              capture_output=True, text=True, timeout=10)

    def sway_json(self, msgtype):
        out = self.sway("-t", msgtype)
        try:
            return json.loads(out.stdout)
        except ValueError:
            return []

    def tree_cons(self):
        """Every con backing an X11 window, with its enclosing workspace."""
        cons = []

        def walk(node, workspace):
            if node.get("type") == "workspace":
                workspace = node
            if node.get("window"):
                cons.append((node, workspace))
            for child in node.get("nodes", []) + node.get("floating_nodes", []):
                walk(child, workspace)

        tree = self.sway_json("get_tree")
        if tree:
            walk(tree, None)
        return cons

    def con_for(self, xid):
        for node, workspace in self.tree_cons():
            if node.get("window") == xid:
                return node, workspace
        return None, None

    # -- X helpers --------------------------------------------------------
    def win(self, xid):
        return self.dpy.create_resource_object("window", xid)

    def window_type(self, xid):
        try:
            prop = self.win(xid).get_full_property(self.a_type, 4)  # XA_ATOM
            return prop.value[0] if prop and prop.value else self.a_type_normal
        except Exception:
            return None

    def managed(self, xid):
        return self.window_type(xid) not in (self.a_type_dock,
                                             self.a_type_desktop, None)

    def stamp_actions(self, xid):
        try:
            self.win(xid).change_property(self.a_allowed, 4, 32, self.a_actions)
            self.dpy.flush()
        except Exception:
            pass

    def set_hidden(self, xid, hidden):
        try:
            w = self.win(xid)
            prop = w.get_full_property(self.a_state, 4)
            atoms = list(prop.value) if prop and prop.value else []
            if hidden and self.a_hidden not in atoms:
                atoms.append(self.a_hidden)
            elif not hidden and self.a_hidden in atoms:
                atoms.remove(self.a_hidden)
            else:
                return
            w.change_property(self.a_state, 4, 32, atoms)
            self.dpy.flush()
        except Exception:
            pass

    def client_geometry(self, xid):
        try:
            w = self.win(xid)
            geo = w.get_geometry()
            pos = w.translate_coords(self.root, 0, 0)
            return -pos.x, -pos.y, geo.width, geo.height
        except Exception:
            return None

    # -- work area --------------------------------------------------------
    def workarea(self, out):
        """Output rectangle minus any dock strut that lands inside it."""
        rect = dict(out["rect"])
        try:
            root_geo = self.root.get_geometry()
            clients = self.root.get_full_property(
                self.dpy.intern_atom("_NET_CLIENT_LIST"), 33)  # XA_WINDOW
            for xid in (clients.value if clients else []):
                if self.window_type(xid) != self.a_type_dock:
                    continue
                strut = self.win(xid).get_full_property(self.a_strut, 6)  # CARDINAL
                if not strut or len(strut.value) < 12:
                    continue
                left, right, top, bottom = strut.value[:4]
                if bottom:
                    span_lo, span_hi = strut.value[10], strut.value[11]
                    if span_lo < rect["x"] + rect["width"] and span_hi > rect["x"]:
                        occupied = bottom - (root_geo.height
                                             - rect["y"] - rect["height"])
                        if occupied > 0:
                            rect["height"] = max(1, rect["height"] - occupied)
                if top:
                    span_lo, span_hi = strut.value[4], strut.value[5]
                    if span_lo < rect["x"] + rect["width"] and span_hi > rect["x"]:
                        occupied = top - rect["y"]
                        if occupied > 0:
                            rect["y"] += occupied
                            rect["height"] = max(1, rect["height"] - occupied)
        except Exception:
            pass
        return rect

    def output_of(self, con_rect):
        outs = [o for o in self.sway_json("get_outputs") if o.get("active")]
        cx = con_rect["x"] + con_rect["width"] // 2
        cy = con_rect["y"] + con_rect["height"] // 2
        for out in outs:
            r = out["rect"]
            if r["x"] <= cx < r["x"] + r["width"] and \
               r["y"] <= cy < r["y"] + r["height"]:
                return out
        return outs[0] if outs else None

    # -- verbs ------------------------------------------------------------
    def apply_rect(self, con_id, xid, x, y, w, h):
        """Place the client area at the given rectangle. sway sizes the
        container, so command once, measure the client, and correct the
        border delta the measurement reveals."""
        self.sway(f"[con_id={con_id}] resize set {w} px {h} px")
        self.sway(f"[con_id={con_id}] move position {x} {y}")
        time.sleep(0.1)
        got = self.client_geometry(xid)
        if not got:
            return
        gx, gy, gw, gh = got
        dx, dy, dw, dh = x - gx, y - gy, w - gw, h - gh
        if any((dx, dy, dw, dh)):
            self.sway(f"[con_id={con_id}] resize set {w + dw} px {h + dh} px")
            self.sway(f"[con_id={con_id}] move position {x + dx} {y + dy}")

    def maximize(self, xid, on):
        con, _ = self.con_for(xid)
        if not con:
            return
        entry = self.state.setdefault(xid, {"restore": None, "min": False})
        if on:
            out = self.output_of(con["rect"])
            if not out:
                return
            if entry["restore"] is None:
                geo = self.client_geometry(xid)
                entry["restore"] = geo or (con["rect"]["x"], con["rect"]["y"],
                                           con["rect"]["width"],
                                           con["rect"]["height"])
            area = self.workarea(out)
            self.apply_rect(con["id"], xid, area["x"], area["y"],
                            area["width"], area["height"])
        else:
            if entry["restore"]:
                self.apply_rect(con["id"], xid, *entry["restore"])
                entry["restore"] = None

    def minimize(self, xid):
        con, _ = self.con_for(xid)
        if not con:
            return
        entry = self.state.setdefault(xid, {"restore": None, "min": False})
        if not entry["min"]:
            if entry["restore"] is None:
                entry["restore"] = self.client_geometry(xid)
            entry["border"] = (con.get("border"),
                               con.get("current_border_width"))
            self.sway(f"[con_id={con['id']}] move scratchpad")
            entry["min"] = True
            self.set_hidden(xid, True)

    def activate(self, xid):
        con, _ = self.con_for(xid)
        if not con:
            return
        entry = self.state.get(xid)
        if entry and entry["min"]:
            self.sway(f"[con_id={con['id']}] scratchpad show")
            entry["min"] = False
            self.set_hidden(xid, False)
            # The scratchpad round-trip resets the border to the floating
            # default; put back the style the window carried in.
            style, width = entry.pop("border", (None, None))
            if style == "pixel":
                self.sway(f"[con_id={con['id']}] border pixel {width or 2}")
            elif style == "none":
                self.sway(f"[con_id={con['id']}] border none")
            if entry["restore"]:
                self.apply_rect(con["id"], xid, *entry["restore"])
                entry["restore"] = None
        self.sway(f"[con_id={con['id']}] focus")

    def place_new(self, xid):
        """Cascade a window sway centred onto an already-centred sibling, the
        way stacking window managers stagger new windows."""
        con, _ = self.con_for(xid)
        if not con or con.get("type") != "floating_con":
            return
        rect = con["rect"]
        out = self.output_of(rect)
        if not out:
            return
        # Centring differs by a titlebar's height across sway generations, so
        # the "sway placed this" test carries a matching slack.
        centred_x = out["rect"]["x"] + (out["rect"]["width"] - rect["width"]) // 2
        centred_y = out["rect"]["y"] + (out["rect"]["height"] - rect["height"]) // 2
        if abs(rect["x"] - centred_x) > 24 or abs(rect["y"] - centred_y) > 24:
            return
        overlaps = any(
            n.get("window") != xid and abs(n["rect"]["x"] - rect["x"]) < 8
            and abs(n["rect"]["y"] - rect["y"]) < 8
            for n, _ in self.tree_cons())
        if not overlaps:
            self.cascade = 0
            return
        self.cascade = (self.cascade % 8) + 1
        step = 32 * self.cascade
        area = self.workarea(out)
        x = min(centred_x + step, area["x"] + max(0, area["width"] - rect["width"]))
        y = min(centred_y + step, area["y"] + max(0, area["height"] - rect["height"]))
        self.sway(f"[con_id={con['id']}] move position {x} {y}")

    def sweep(self):
        """No window may live on a workspace no output shows: after an output
        is unplugged sway re-homes the workspace but does not surface it."""
        visible = {ws["output"]: ws["name"]
                   for ws in self.sway_json("get_workspaces") if ws.get("visible")}
        if not visible:
            return
        for node, workspace in self.tree_cons():
            if not workspace or workspace.get("name") in visible.values():
                continue
            xid = node.get("window")
            entry = self.state.get(xid)
            if entry and entry["min"]:
                continue
            out = self.output_of(node["rect"])
            target = visible.get(out["name"]) if out else None
            if target is None:
                target = next(iter(visible.values()))
            self.sway(f"[con_id={node['id']}] move container to workspace {target}")
            if out:
                area = self.workarea(out)
                rect = node["rect"]
                x = max(area["x"], min(rect["x"],
                        area["x"] + area["width"] - rect["width"]))
                y = max(area["y"], min(rect["y"],
                        area["y"] + area["height"] - rect["height"]))
                self.sway(f"[con_id={node['id']}] move position {x} {y}")

    # -- event loops ------------------------------------------------------
    def handle_x_event(self, ev):
        if ev.type == X.MapNotify and not ev.override:
            xid = ev.window.id
            if self.managed(xid):
                self.stamp_actions(xid)
                time.sleep(0.15)
                self.place_new(xid)
        elif ev.type == X.DestroyNotify:
            self.state.pop(ev.window.id, None)
        elif ev.type == X.ClientMessage:
            xid = ev.window.id
            if ev.client_type == self.a_state and self.managed(xid):
                action, p1, p2 = ev.data[1][0], ev.data[1][1], ev.data[1][2]
                if {p1, p2} & {self.a_max_v, self.a_max_h}:
                    if action == 2:
                        entry = self.state.get(xid)
                        action = 0 if entry and entry["restore"] else 1
                    self.maximize(xid, action == 1)
            elif ev.client_type == self.a_change_state and ev.data[1][0] == 3:
                if self.managed(xid):
                    self.minimize(xid)
            elif ev.client_type == self.a_active and self.managed(xid):
                self.activate(xid)

    def _subscribe(self):
        """Window and output events. sway 1.7's IPC predates the output event
        type and rejects the pair outright, so fall back to window events
        alone — its sessions hold one screen, which cannot strand windows."""
        for payload in ('["window","output"]', '["window"]'):
            sub = subprocess.Popen(
                ["swaymsg", "-t", "subscribe", "-m", payload],
                env=self.env, stdout=subprocess.PIPE, text=True)
            time.sleep(0.3)
            if sub.poll() is None:
                return sub
            sub.stdout.close()
        raise RuntimeError("sway IPC refused event subscription")

    def run(self):
        sub = self._subscribe()
        x_fd = self.dpy.fileno()
        sway_fd = sub.stdout.fileno()
        self.sweep()
        while True:
            if sub.poll() is not None:
                raise RuntimeError("sway IPC subscription closed")
            ready, _, _ = select.select([x_fd, sway_fd], [], [], 3.0)
            if sway_fd in ready:
                sub.stdout.readline()
                # Events land in bursts (an output change evacuates a whole
                # workspace); let sway settle before sweeping once.
                time.sleep(0.3)
                while select.select([sway_fd], [], [], 0.2)[0]:
                    sub.stdout.readline()
                self.sweep()
            if x_fd in ready:
                while self.dpy.pending_events():
                    self.handle_x_event(self.dpy.next_event())


def main():
    while True:
        sock = find_swaysock()
        if not sock:
            time.sleep(3)
            continue
        try:
            Bridge(sock).run()
        except Exception as exc:
            print(f"[wmbridge] restarting: {exc}", file=sys.stderr, flush=True)
            time.sleep(3)


if __name__ == "__main__":
    main()
