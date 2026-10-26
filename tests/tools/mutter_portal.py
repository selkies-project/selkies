#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""RemoteDesktop and ScreenCast portal backend that hands each session to Mutter's own D-Bus API.

It forwards the way xdg-desktop-portal-gnome does, without its dialog: every request is granted,
and a session records Mutter's primary monitor. One difference: Mutter reads absolute pointer and
touch positions in the stream's pixels, while the spec and the portal frontend have them in the
stream's logical size, so this backend scales them by the monitor's scale. GNOME's passes them on
unchanged, which lands them at 1/scale on a scaled monitor; converting them here, as a fixed
backend would, leaves the suite checking the client.
`--no-eis` refuses `ConnectToEIS`, as a backend without EIS does, so a client falls back to the
Notify methods. `--scale N` first sets the primary monitor's scale through Mutter's DisplayConfig
(Mutter 46 has no `gdctl`). Runs on the system Python with PyGObject; the portal frontend finds
it through a `.portal` file naming BUS_NAME.

Usage: mutter_portal.py [--no-eis] [--scale N]
"""
import sys
import time

from gi.repository import Gio, GLib

BUS_NAME = "org.freedesktop.impl.portal.desktop.mutter_test"
PORTAL_PATH = "/org/freedesktop/portal/desktop"
RD_NAME, RD_PATH = "org.gnome.Mutter.RemoteDesktop", "/org/gnome/Mutter/RemoteDesktop"
SC_NAME, SC_PATH = "org.gnome.Mutter.ScreenCast", "/org/gnome/Mutter/ScreenCast"
RD_SESSION, SC_SESSION = RD_NAME + ".Session", SC_NAME + ".Session"
SC_STREAM = SC_NAME + ".Stream"
# Mutter's NotifyPointerAxis flag for a finished scroll.
AXIS_FINISH = 1
# The portal's cursor mode bits (hidden, embedded, metadata) as Mutter's cursor-mode values.
CURSOR_MODES = {1: 0, 2: 1, 4: 2}

SESSION_XML = """<node><interface name="org.freedesktop.impl.portal.Session">
  <method name="Close"/><signal name="Closed"/><property name="version" type="u" access="read"/>
</interface></node>"""

PORTAL_XML = """<node>
<interface name="org.freedesktop.impl.portal.ScreenCast">
  <method name="CreateSession"><arg type="o" direction="in"/><arg type="o" direction="in"/>
    <arg type="s" direction="in"/><arg type="a{sv}" direction="in"/>
    <arg type="u" direction="out"/><arg type="a{sv}" direction="out"/></method>
  <method name="SelectSources"><arg type="o" direction="in"/><arg type="o" direction="in"/>
    <arg type="s" direction="in"/><arg type="a{sv}" direction="in"/>
    <arg type="u" direction="out"/><arg type="a{sv}" direction="out"/></method>
  <method name="Start"><arg type="o" direction="in"/><arg type="o" direction="in"/>
    <arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="a{sv}" direction="in"/>
    <arg type="u" direction="out"/><arg type="a{sv}" direction="out"/></method>
  <property name="AvailableSourceTypes" type="u" access="read"/>
  <property name="AvailableCursorModes" type="u" access="read"/>
  <property name="version" type="u" access="read"/>
</interface>
<interface name="org.freedesktop.impl.portal.RemoteDesktop">
  <method name="CreateSession"><arg type="o" direction="in"/><arg type="o" direction="in"/>
    <arg type="s" direction="in"/><arg type="a{sv}" direction="in"/>
    <arg type="u" direction="out"/><arg type="a{sv}" direction="out"/></method>
  <method name="SelectDevices"><arg type="o" direction="in"/><arg type="o" direction="in"/>
    <arg type="s" direction="in"/><arg type="a{sv}" direction="in"/>
    <arg type="u" direction="out"/><arg type="a{sv}" direction="out"/></method>
  <method name="Start"><arg type="o" direction="in"/><arg type="o" direction="in"/>
    <arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="a{sv}" direction="in"/>
    <arg type="u" direction="out"/><arg type="a{sv}" direction="out"/></method>
  <method name="NotifyPointerMotion"><arg type="o" direction="in"/><arg type="a{sv}" direction="in"/>
    <arg type="d" direction="in"/><arg type="d" direction="in"/></method>
  <method name="NotifyPointerMotionAbsolute"><arg type="o" direction="in"/><arg type="a{sv}" direction="in"/>
    <arg type="u" direction="in"/><arg type="d" direction="in"/><arg type="d" direction="in"/></method>
  <method name="NotifyPointerButton"><arg type="o" direction="in"/><arg type="a{sv}" direction="in"/>
    <arg type="i" direction="in"/><arg type="u" direction="in"/></method>
  <method name="NotifyPointerAxis"><arg type="o" direction="in"/><arg type="a{sv}" direction="in"/>
    <arg type="d" direction="in"/><arg type="d" direction="in"/></method>
  <method name="NotifyPointerAxisDiscrete"><arg type="o" direction="in"/><arg type="a{sv}" direction="in"/>
    <arg type="u" direction="in"/><arg type="i" direction="in"/></method>
  <method name="NotifyKeyboardKeycode"><arg type="o" direction="in"/><arg type="a{sv}" direction="in"/>
    <arg type="i" direction="in"/><arg type="u" direction="in"/></method>
  <method name="NotifyKeyboardKeysym"><arg type="o" direction="in"/><arg type="a{sv}" direction="in"/>
    <arg type="i" direction="in"/><arg type="u" direction="in"/></method>
  <method name="NotifyTouchDown"><arg type="o" direction="in"/><arg type="a{sv}" direction="in"/>
    <arg type="u" direction="in"/><arg type="u" direction="in"/><arg type="d" direction="in"/>
    <arg type="d" direction="in"/></method>
  <method name="NotifyTouchMotion"><arg type="o" direction="in"/><arg type="a{sv}" direction="in"/>
    <arg type="u" direction="in"/><arg type="u" direction="in"/><arg type="d" direction="in"/>
    <arg type="d" direction="in"/></method>
  <method name="NotifyTouchUp"><arg type="o" direction="in"/><arg type="a{sv}" direction="in"/>
    <arg type="u" direction="in"/></method>
  <method name="ConnectToEIS"><arg type="o" direction="in"/><arg type="s" direction="in"/>
    <arg type="a{sv}" direction="in"/><arg type="h" direction="out"/></method>
  <property name="AvailableDeviceTypes" type="u" access="read"/>
  <property name="version" type="u" access="read"/>
</interface>
</node>"""

PROPERTIES = {
    "org.freedesktop.impl.portal.ScreenCast": {
        # Monitors; hidden, embedded, and metadata cursors.
        "AvailableSourceTypes": GLib.Variant("u", 1),
        "AvailableCursorModes": GLib.Variant("u", 7),
        "version": GLib.Variant("u", 5),
    },
    "org.freedesktop.impl.portal.RemoteDesktop": {
        # Keyboard and pointer.
        "AvailableDeviceTypes": GLib.Variant("u", 3),
        "version": GLib.Variant("u", 2),
    },
}


class Session:
    def __init__(self, handle: str, remote: bool) -> None:
        self.handle = handle
        self.remote = remote
        self.rd_path = ""
        self.sc_path = ""
        self.stream_path = ""
        self.node_id = 0
        self.devices = 3
        self.cursor_mode = 0
        self.scale = 1.0
        self.reg_id = 0
        self.signal_id = 0


class Backend:
    def __init__(self, conn: Gio.DBusConnection, eis: bool) -> None:
        self.conn = conn
        self.eis = eis
        self.sessions: dict = {}
        self.session_info = Gio.DBusNodeInfo.new_for_xml(SESSION_XML).interfaces[0]
        for info in Gio.DBusNodeInfo.new_for_xml(PORTAL_XML).interfaces:
            conn.register_object(PORTAL_PATH, info, self.method_call, self.get_property, None)

    def call(self, name: str, path: str, iface: str, method: str, args, reply: str = None,
             fds: Gio.UnixFDList = None):
        params = GLib.Variant(args[0], args[1]) if args else None
        if reply == "(h)":
            return self.conn.call_with_unix_fd_list_sync(
                name, path, iface, method, params, GLib.VariantType(reply),
                Gio.DBusCallFlags.NONE, -1, fds, None)
        return self.conn.call_sync(name, path, iface, method, params,
                                   GLib.VariantType(reply) if reply else None,
                                   Gio.DBusCallFlags.NONE, -1, None)

    def get_property(self, conn, sender, path, iface, name):
        if iface == "org.freedesktop.impl.portal.Session":
            return GLib.Variant("u", 1)
        return PROPERTIES[iface][name]

    def create_session(self, handle: str, remote: bool) -> None:
        s = Session(handle, remote)
        if remote:
            (s.rd_path,) = self.call(RD_NAME, RD_PATH, RD_NAME, "CreateSession", None, "(o)").unpack()
            sid = self.call(RD_NAME, s.rd_path, "org.freedesktop.DBus.Properties", "Get",
                            ("(ss)", (RD_SESSION, "SessionId")), "(v)").unpack()[0]
            props = {"remote-desktop-session-id": GLib.Variant("s", sid)}
        else:
            props = {}
        (s.sc_path,) = self.call(SC_NAME, SC_PATH, SC_NAME, "CreateSession",
                                 ("(a{sv})", (props,)), "(o)").unpack()
        s.reg_id = self.conn.register_object(
            handle, self.session_info, self.session_call, self.get_property, None)
        self.sessions[handle] = s

    def monitor_scale(self, position: tuple) -> float:
        """The scale of Mutter's logical monitor at `position`."""
        state = self.call("org.gnome.Mutter.DisplayConfig", "/org/gnome/Mutter/DisplayConfig",
                          "org.gnome.Mutter.DisplayConfig", "GetCurrentState", None).unpack()
        return next((m[2] for m in state[2] if (m[0], m[1]) == position), 1.0)

    def close_session(self, s: Session) -> None:
        self.sessions.pop(s.handle, None)
        if s.signal_id:
            self.conn.signal_unsubscribe(s.signal_id)
        if s.reg_id:
            self.conn.unregister_object(s.reg_id)
        try:
            if s.remote:
                self.call(RD_NAME, s.rd_path, RD_SESSION, "Stop", None)
            else:
                self.call(SC_NAME, s.sc_path, SC_SESSION, "Stop", None)
        except GLib.Error:
            pass

    def session_call(self, conn, sender, path, iface, method, params, invocation):
        s = self.sessions.get(path)
        if s:
            self.close_session(s)
        invocation.return_value(None)

    def start(self, s: Session, invocation: Gio.DBusMethodInvocation) -> None:
        """Record the primary monitor, start, and answer once Mutter names the stream's node."""
        (s.stream_path,) = self.call(SC_NAME, s.sc_path, SC_SESSION, "RecordMonitor",
                                     ("(sa{sv})", ("", {"cursor-mode": GLib.Variant("u", s.cursor_mode)})),
                                     "(o)").unpack()

        def added(conn, sender, path, iface, signal, params):
            s.node_id = params.unpack()[0]
            conn.signal_unsubscribe(s.signal_id)
            s.signal_id = 0
            p = self.call(SC_NAME, s.stream_path, "org.freedesktop.DBus.Properties", "Get",
                          ("(ss)", (SC_STREAM, "Parameters")), "(v)").unpack()[0]
            props = {"id": GLib.Variant("s", "0"), "source_type": GLib.Variant("u", 1)}
            if "position" in p:
                props["position"] = GLib.Variant("(ii)", tuple(p["position"]))
            if "size" in p:
                props["size"] = GLib.Variant("(ii)", tuple(p["size"]))
            if "mapping-id" in p:
                props["mapping_id"] = GLib.Variant("s", p["mapping-id"])
            s.scale = self.monitor_scale(tuple(p.get("position", (0, 0))))
            print(f"mutter_portal: stream {s.node_id}: Mutter's parameters {p}, scale {s.scale}", flush=True)
            results = {"streams": GLib.Variant("a(ua{sv})", [(s.node_id, props)])}
            if s.remote:
                results["devices"] = GLib.Variant("u", s.devices)
            invocation.return_value(GLib.Variant("(ua{sv})", (0, results)))

        s.signal_id = self.conn.signal_subscribe(
            SC_NAME, SC_STREAM, "PipeWireStreamAdded", s.stream_path, None,
            Gio.DBusSignalFlags.NONE, added)
        if s.remote:
            self.call(RD_NAME, s.rd_path, RD_SESSION, "Start", None)
        else:
            self.call(SC_NAME, s.sc_path, SC_SESSION, "Start", None)

    def method_call(self, conn, sender, path, iface, method, params, invocation):
        args = params.unpack()
        remote = iface.endswith("RemoteDesktop")
        try:
            if method == "CreateSession":
                self.create_session(args[1], remote)
                invocation.return_value(GLib.Variant("(ua{sv})", (0, {})))
                return
            if method in ("SelectSources", "SelectDevices"):
                s = self.sessions[args[1]]
                if method == "SelectSources":
                    s.cursor_mode = CURSOR_MODES.get(args[3].get("cursor_mode"), s.cursor_mode)
                else:
                    s.devices = args[3].get("types", s.devices) & 3 or 3
                invocation.return_value(GLib.Variant("(ua{sv})", (0, {})))
                return
            if method == "Start":
                self.start(self.sessions[args[1]], invocation)
                return
            s = self.sessions[args[0]]
            if method == "ConnectToEIS":
                if not self.eis:
                    invocation.return_dbus_error("org.freedesktop.portal.Error.NotAllowed",
                                                 "this backend offers no EIS")
                    return
                reply, fds = self.call(RD_NAME, s.rd_path, RD_SESSION, "ConnectToEIS",
                                       ("(a{sv})", ({"device-types": GLib.Variant("u", s.devices)},)), "(h)")
                invocation.return_value_with_unix_fd_list(reply, fds)
                return
            forward = {
                "NotifyPointerMotion": ("NotifyPointerMotionRelative", "(dd)", lambda a: (a[2], a[3])),
                "NotifyPointerMotionAbsolute": ("NotifyPointerMotionAbsolute", "(sdd)",
                                                lambda a: (s.stream_path, a[3] * s.scale, a[4] * s.scale)),
                "NotifyPointerButton": ("NotifyPointerButton", "(ib)", lambda a: (a[2], bool(a[3]))),
                "NotifyPointerAxis": ("NotifyPointerAxis", "(ddu)",
                                      lambda a: (a[2], a[3], AXIS_FINISH if a[1].get("finish") else 0)),
                "NotifyPointerAxisDiscrete": ("NotifyPointerAxisDiscrete", "(ui)", lambda a: (a[2], a[3])),
                "NotifyKeyboardKeycode": ("NotifyKeyboardKeycode", "(ub)", lambda a: (a[2], bool(a[3]))),
                "NotifyKeyboardKeysym": ("NotifyKeyboardKeysym", "(ub)", lambda a: (a[2], bool(a[3]))),
                "NotifyTouchDown": ("NotifyTouchDown", "(sudd)",
                                    lambda a: (s.stream_path, a[3], a[4] * s.scale, a[5] * s.scale)),
                "NotifyTouchMotion": ("NotifyTouchMotion", "(sudd)",
                                      lambda a: (s.stream_path, a[3], a[4] * s.scale, a[5] * s.scale)),
                "NotifyTouchUp": ("NotifyTouchUp", "(u)", lambda a: (a[2],)),
            }[method]
            self.call(RD_NAME, s.rd_path, RD_SESSION, forward[0], (forward[1], forward[2](args)))
            invocation.return_value(None)
        except (GLib.Error, KeyError) as e:
            print(f"mutter_portal: {method} failed: {e}", file=sys.stderr, flush=True)
            invocation.return_dbus_error("org.freedesktop.portal.Error.Failed", str(e))


def wait_for_names(conn: Gio.DBusConnection, names: tuple, timeout: float = 30.0) -> None:
    """Wait until Mutter owns each of `names`: it opens its Wayland socket before it takes them
    on the bus, by seconds where a session service it asks first times out."""
    deadline = time.monotonic() + timeout
    for name in names:
        while not conn.call_sync("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus",
                                 "NameHasOwner", GLib.Variant("(s)", (name,)), GLib.VariantType("(b)"),
                                 Gio.DBusCallFlags.NONE, -1, None).unpack()[0]:
            if time.monotonic() > deadline:
                raise TimeoutError(f"{name} never appeared on the bus")
            time.sleep(0.1)


def set_scale(conn: Gio.DBusConnection, scale: float) -> None:
    """Put the primary monitor alone at `scale` in its current mode, until Mutter exits."""
    state = conn.call_sync("org.gnome.Mutter.DisplayConfig", "/org/gnome/Mutter/DisplayConfig",
                           "org.gnome.Mutter.DisplayConfig", "GetCurrentState", None, None,
                           Gio.DBusCallFlags.NONE, -1, None).unpack()
    serial, monitors = state[0], state[1]
    (connector, _, _, _), modes, _ = monitors[0]
    mode = next(m[0] for m in modes if m[6].get("is-current")) if modes else ""
    # Method 1 applies the layout without saving it.
    conn.call_sync("org.gnome.Mutter.DisplayConfig", "/org/gnome/Mutter/DisplayConfig",
                   "org.gnome.Mutter.DisplayConfig", "ApplyMonitorsConfig",
                   GLib.Variant("(uua(iiduba(ssa{sv}))a{sv})",
                                (serial, 1, [(0, 0, scale, 0, True, [(connector, mode, {})])], {})),
                   None, Gio.DBusCallFlags.NONE, -1, None)


def main() -> None:
    conn = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    argv = sys.argv[1:]
    wait_for_names(conn, (RD_NAME, SC_NAME, "org.gnome.Mutter.DisplayConfig"))
    if "--scale" in argv:
        set_scale(conn, float(argv[argv.index("--scale") + 1]))
    backend = Backend(conn, eis="--no-eis" not in argv)
    loop = GLib.MainLoop()
    Gio.bus_own_name_on_connection(conn, BUS_NAME, Gio.BusNameOwnerFlags.NONE,
                                   lambda *a: print("mutter_portal: ready", flush=True),
                                   lambda *a: loop.quit())
    loop.run()
    del backend


if __name__ == "__main__":
    main()
