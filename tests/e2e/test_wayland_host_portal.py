#!/usr/bin/env python3
"""Host capture of a compositor without the wlroots protocols goes through xdg-desktop-portal.

A KDE session offers neither screencopy nor the virtual keyboard and pointer protocols to
ordinary clients, so `SELKIES_WAYLAND_HOST_DISPLAY` aimed at its socket has pixelflux open a
RemoteDesktop portal session instead: the monitor's PipeWire stream carries the frames and the
seat's input goes through the portal by keysym and stream coordinates. This suite runs a real
KDE stack on a private bus — `kwin_wayland --virtual`, PipeWire, xdg-desktop-portal, and its KDE
backend — starts the server against it over either transport, and checks the rung was taken,
the stream negotiated, and that a browser's pointer moves, clicks, scrolls, and keys land on a
client window inside KWin. Plasma 6.6's virtual backend streams its frames as GPU dmabufs; KWin
5's emits no output damage and streams none, and the frame check reports skipped there. The
pointer lands where the page points: the browser sends the stream's pixels, which the injector
takes into the compositor's logical space. The `scaled` block runs KWin's output at scale 2,
where the stream's pixels are twice the logical size and an injector that skipped that step, or
took it twice, lands twice or half as far.

The `gnome` blocks run the same checks against headless Mutter, with a stand-in for GNOME's
portal backend that forwards to Mutter's own API as GNOME's does (tests/tools/mutter_portal.py):
`gnome` at scale 1, `gnome-scaled` at scale 2 over libei, and `gnome-notify` at scale 2 with
the backend refusing EIS, where the client's logical point reaches Mutter as the stream's pixels.
Mutter offers the cursor metadata at a fixed size, which the stream must accept for any sprite to
arrive.

Usage: python3 tests/e2e/test_wayland_host_portal.py
    [websockets|webrtc|scaled|gnome|gnome-scaled|gnome-notify]
"""
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright

SIZE = (1280, 720)
# The box the page maps the pointer through, with the stream's size in pixels: the stream element
# once that has a size, else the input overlay (a WebRTC `<video>` has none before a frame).
SINK_JS = """() => {
  const canvas = document.getElementById('videoCanvas');
  const video = document.getElementById('stream');
  const px = (canvas && canvas.width > 0) ? [canvas.width, canvas.height]
             : (video && video.videoWidth > 0) ? [video.videoWidth, video.videoHeight] : null;
  for (const id of px ? ['videoCanvas', 'videoWorkerCanvas', 'videoStream', 'stream'] : ['overlayInput']) {
    const el = document.getElementById(id);
    const r = el && el.getBoundingClientRect ? el.getBoundingClientRect() : null;
    if (r && r.width > 0) return {x: r.left, y: r.top, cssW: r.width, cssH: r.height, px};
  }
  return null;
}"""


def service_exec(name: str) -> str:
    """The executable behind a D-Bus service name, from its activation file, the way the bus
    itself finds it; empty when no data directory carries the file."""
    dirs = os.environ.get("XDG_DATA_DIRS", "/usr/local/share:/usr/share").split(":")
    for base in dirs:
        path = os.path.join(base, "dbus-1", "services", f"{name}.service")
        try:
            with open(path) as f:
                for line in f:
                    if line.startswith("Exec="):
                        return line[5:].strip().split()[0]
        except OSError:
            continue
    return ""


class PortalRig:
    """A desktop's compositor, PipeWire, and xdg-desktop-portal on a private session bus and
    runtime dir, the way the desktop's session exposes them; `socket_path` is the compositor's
    socket. A compositor's output is SIZE in logical pixels at the rig's scale, so its mode is
    SIZE times the scale."""

    name = ""
    # The processes whose logs the suite runner keeps, by the name each was spawned under.
    logs: tuple = ()

    def __init__(self, socket_name: str, scale: int, **env: str) -> None:
        # A short path: the compositor's, PipeWire's, and the bus's sockets all live under it.
        self.root = tempfile.mkdtemp(prefix="pf-portal-")
        self.runtime = os.path.join(self.root, "rt")
        os.makedirs(self.runtime, mode=0o700)
        self.socket_name = socket_name
        self.socket_path = os.path.join(self.runtime, self.socket_name)
        self.procs: list = []
        self.bus = ""
        self.bus_pid = 0
        self.portal = service_exec("org.freedesktop.portal.Desktop")
        self.env = {
            "PATH": os.environ.get("PATH", ""), "HOME": os.path.expanduser("~"),
            "XDG_RUNTIME_DIR": self.runtime, "WAYLAND_DISPLAY": self.socket_name,
            "XDG_SESSION_TYPE": "wayland", "XDG_CONFIG_HOME": os.path.join(self.root, "config"), **env,
        }
        self.scale = scale
        self.mode = (SIZE[0] * scale, SIZE[1] * scale)
        os.makedirs(self.env["XDG_CONFIG_HOME"])

    def _spawn(self, cmd: list, name: str, **extra: str) -> subprocess.Popen:
        log = open(os.path.join(self.root, f"{name}.log"), "w")
        proc = H.spawn(cmd, env={**self.env, **extra}, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        self.procs.append(proc)
        return proc

    def _start_bus(self, binaries: tuple) -> str:
        """Check for the stack's binaries, then start the bus, PipeWire, and WirePlumber; returns
        why it could not, or an empty string."""
        for binary in ("dbus-daemon", "pipewire", "wireplumber") + binaries:
            if not shutil.which(binary):
                return f"{binary} not installed"
        if not self.portal or not os.path.exists(self.portal):
            return "xdg-desktop-portal not installed"
        bus_proc = subprocess.Popen(["dbus-daemon", "--session", "--print-address=1", "--print-pid=1", "--fork",
                                     "--address", f"unix:path={self.root}/bus"],
                                    stdout=subprocess.PIPE, text=True, env=self.env)
        printed = (bus_proc.stdout.read() or "").split()
        self.bus = next((w for w in printed if "=" in w), "")
        self.bus_pid = next((int(w) for w in printed if w.isdigit()), 0)
        if not self.bus:
            return "dbus-daemon printed no address"
        self.env["DBUS_SESSION_BUS_ADDRESS"] = self.bus
        self._spawn(["pipewire"], "pipewire")
        time.sleep(0.5)
        self._spawn(["wireplumber"], "wireplumber")
        return ""

    def _wait_socket(self) -> str:
        deadline = time.time() + 20
        while not os.path.exists(self.socket_path):
            if time.time() > deadline:
                return f"{self.name} never opened its socket: {H.tail(self.log(self.logs[0]), 3)}"
            time.sleep(0.2)
        return ""

    def log(self, name: str) -> str:
        return os.path.join(self.root, f"{name}.log")

    def stop(self) -> None:
        for proc in reversed(self.procs):
            try:
                os.killpg(proc.pid, 15)
            except OSError:
                pass
        time.sleep(1.0)
        for proc in self.procs:
            try:
                os.killpg(proc.pid, 9)
            except OSError:
                pass
        if self.bus_pid:
            try:
                os.kill(self.bus_pid, 15)
            except OSError:
                pass
        # The stack's logs go where the suite runner keeps a suite's record.
        for name in self.logs:
            try:
                shutil.copy2(self.log(name), os.path.join(H.WORKDIR, f"portal-{name}.log"))
            except OSError:
                pass
        shutil.rmtree(self.root, ignore_errors=True)


class KdeRig(PortalRig):
    """kwin_wayland (virtual backend) and the KDE portal backend.

    KWin runs with `KWIN_WAYLAND_NO_PERMISSION_CHECKS`, as the KDE image sets it: KWin 6 grants
    its restricted globals, the screencast one among them, by the desktop file of the client's
    executable, which it does not resolve in this bare session (it logs the backend as not
    listing `zkde_screencast_unstable_v1`, which the backend's desktop file does), and the backend
    then refuses every session request. KDE 6's backend also asks before a remote-desktop session
    starts unless the permission store's `kde-authorized` table grants the app `remote-desktop`,
    so the rig grants it to a host app, whose id is empty, as a deployment does ahead of time.

    KWin's virtual output takes its mode from `--width` and `--height` times `--scale`. KWin 5
    takes the output's scale from `--scale` as well; KWin 6 takes it from the
    `kwinoutputconfig.json` it keeps, else picks its own (1 here), so the rig also writes one into
    a configuration directory of its own."""

    name = "KWin"
    logs = ("kwin", "pipewire", "wireplumber", "portal-kde", "portal")

    def __init__(self, scale: int = 1) -> None:
        super().__init__("wayland-portal", scale, XDG_CURRENT_DESKTOP="KDE", QT_QPA_PLATFORM="wayland")
        self.portal_kde = service_exec("org.freedesktop.impl.portal.desktop.kde")
        with open(os.path.join(self.env["XDG_CONFIG_HOME"], "kwinoutputconfig.json"), "w") as f:
            json.dump([
                {"name": "outputs", "data": [{"connectorName": "Virtual-0", "scale": scale, "mode": {
                    "width": self.mode[0], "height": self.mode[1], "refreshRate": 60000}}]},
                {"name": "setups", "data": [{"lidClosed": False, "outputs": [
                    {"enabled": True, "outputIndex": 0, "position": {"x": 0, "y": 0}, "priority": 0}]}]},
            ], f)

    def start(self) -> str:
        """Bring the stack up; returns why it could not, or an empty string."""
        if not self.portal_kde or not os.path.exists(self.portal_kde):
            return "xdg-desktop-portal-kde not installed"
        why = self._start_bus(("kwin_wayland", "gdbus"))
        if why:
            return why
        # KWin runs from a copy: distributions give kwin_wayland CAP_SYS_NICE as a file capability,
        # which a container whose bounding set lacks it cannot exec, and the rig needs none
        # (shutil.copy leaves the capability's attribute behind). The copy keeps the name, which
        # KWin's platform plugin requires of the executable.
        kwin = os.path.join(self.root, "kwin_wayland")
        shutil.copy(shutil.which("kwin_wayland"), kwin)
        self._spawn([kwin, "--virtual", "--width", str(SIZE[0]), "--height", str(SIZE[1]),
                     "--scale", str(self.scale), "--no-lockscreen", "--no-global-shortcuts", "--no-kactivities",
                     "--socket", self.socket_name], "kwin", KWIN_WAYLAND_NO_PERMISSION_CHECKS="1",
                    # KWin 6's Qt would register with the host portal as it starts, which has the
                    # bus start the portal and a KDE backend before KWin is up: that backend holds
                    # the name the rig's own needs and refuses every session.
                    QT_NO_XDG_DESKTOP_PORTAL="1")
        why = self._wait_socket()
        if why:
            return why
        time.sleep(1.0)
        # KWin casts the screen through its screencast plugin, and only while it composites with
        # OpenGL: KWin 5 does on Mesa's surfaceless platform, KWin 6 on a render node alone, which
        # a host without a GPU gets from the vgem module.
        deadline = time.time() + 10
        while True:
            plugins = self.kwin_property("/Plugins", "org.kde.KWin.Plugins", "LoadedPlugins")
            compositing = self.kwin_property("/Compositor", "org.kde.kwin.Compositing", "compositingType")
            if (plugins and compositing and "'none'" not in compositing) or time.time() > deadline:
                break
            time.sleep(0.5)
        if "screencast'" not in plugins:  # 'screencast', KWin 5's 'kwin5_plugin_screencast'
            return "KWin loaded no screencast plugin (Debian and Ubuntu ship KWin 6's in kwin-common)"
        if "gl" not in compositing:
            kind = compositing.strip("(<>,)'") or "nothing"
            return (f"KWin composites with {kind} and casts the screen only with OpenGL, which needs a "
                    "render node (the vgem module gives one)")
        self._spawn([self.portal_kde], "portal-kde", KDE_FULL_SESSION="true")
        self._spawn([self.portal, "-r"], "portal")
        time.sleep(2.0)
        grant = subprocess.run(
            ["gdbus", "call", "--session", "--dest", "org.freedesktop.impl.portal.PermissionStore",
             "--object-path", "/org/freedesktop/impl/portal/PermissionStore",
             "--method", "org.freedesktop.impl.portal.PermissionStore.SetPermission",
             "kde-authorized", "true", "remote-desktop", "", "['yes']"],
            env=self.env, capture_output=True, text=True, timeout=15)
        if grant.returncode != 0:
            return f"the permission store took no remote-desktop grant: {grant.stderr.strip()}"
        return ""

    def kwin_property(self, path: str, interface: str, name: str) -> str:
        """A property KWin publishes on the rig's bus, as gdbus prints it; empty when none answers."""
        q = subprocess.run(["gdbus", "call", "--session", "--dest", "org.kde.KWin", "--object-path", path,
                            "--method", "org.freedesktop.DBus.Properties.Get", interface, name],
                           env=self.env, capture_output=True, text=True, timeout=10)
        return q.stdout.strip() if q.returncode == 0 else ""


class GnomeRig(PortalRig):
    """Headless Mutter, with tests/tools/mutter_portal.py as the portal's RemoteDesktop and
    ScreenCast backend. GNOME's own backend asks through a dialog that nothing answers headless;
    the stand-in grants every session and forwards it to Mutter's D-Bus API as GNOME's does, but
    converts an absolute position from the stream's logical size to the pixels Mutter reads it
    in, which GNOME's does not. `eis=False` has it refuse `ConnectToEIS`, which leaves the client
    on the Notify methods.

    Mutter's virtual monitor takes the mode; the stand-in then sets the scale, under
    `scale-monitor-framebuffer`, the layout GNOME sessions run scaled with. `E2E_MUTTER` names
    another mutter executable."""

    name = "Mutter"
    logs = ("mutter", "pipewire", "wireplumber", "portal-gnome", "portal")

    def __init__(self, scale: int = 1, eis: bool = True) -> None:
        super().__init__("wayland-gnome", scale, XDG_CURRENT_DESKTOP="GNOME", GSETTINGS_BACKEND="keyfile")
        self.eis = eis
        self.mutter = os.environ.get("E2E_MUTTER") or shutil.which("mutter") or ""
        # The portal takes its backends, and with this set its portals.conf too, from this
        # directory alone.
        self.env["XDG_DESKTOP_PORTAL_DIR"] = os.path.join(self.root, "portals")
        files = {
            os.path.join(self.env["XDG_CONFIG_HOME"], "glib-2.0", "settings", "keyfile"):
                "[org/gnome/mutter]\nexperimental-features=['scale-monitor-framebuffer']\n",
            os.path.join(self.env["XDG_DESKTOP_PORTAL_DIR"], "portals.conf"): "[preferred]\ndefault=mutter_test\n",
            os.path.join(self.env["XDG_DESKTOP_PORTAL_DIR"], "mutter_test.portal"):
                "[portal]\nDBusName=org.freedesktop.impl.portal.desktop.mutter_test\n"
                "Interfaces=org.freedesktop.impl.portal.RemoteDesktop;org.freedesktop.impl.portal.ScreenCast;\n",
        }
        for path, text in files.items():
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "w") as f:
                f.write(text)

    def start(self) -> str:
        """Bring the stack up; returns why it could not, or an empty string."""
        if not self.mutter:
            return "mutter not installed"
        gi = subprocess.run(["/usr/bin/python3", "-I", "-c", "from gi.repository import Gio"],
                            capture_output=True, text=True)
        if gi.returncode != 0:
            return "the system Python has no PyGObject (python3-gi)"
        why = self._start_bus(())
        if why:
            return why
        self._spawn([self.mutter, "--wayland", "--no-x11", "--headless", "--wayland-display", self.socket_name,
                     "--virtual-monitor", f"{self.mode[0]}x{self.mode[1]}"], "mutter")
        why = self._wait_socket()
        if why:
            return why
        backend = [os.path.join(H.TOOLS, "mutter_portal.py")] + ([] if self.eis else ["--no-eis"])
        if self.scale != 1:
            backend += ["--scale", str(self.scale)]
        self._spawn(["/usr/bin/python3", "-I", *backend], "portal-gnome")
        # Up to the backend's own wait for Mutter's bus names, and some.
        deadline = time.time() + 40
        while "ready" not in open(self.log("portal-gnome")).read():
            if time.time() > deadline:
                return f"the Mutter portal backend never came up: {H.tail(self.log('portal-gnome'), 3)}"
            time.sleep(0.2)
        self._spawn([self.portal, "-r"], "portal")
        time.sleep(2.0)
        return ""


def wait_video(page, mode: str, timeout: float):
    return C.wait_ws_video(page, timeout=timeout) if mode == "websockets" else C.wait_wr_video(page, timeout=timeout)


def landing(page, obs, x: float, y: float, logical: tuple, mode: tuple) -> tuple:
    """Move the page's pointer to (x, y); where the session's client saw it last and where it should
    have: the stream pixel the page point shows, in the output's `logical` size. The page maps
    by the stream's size once it has one, so that has to be the host's `mode` first."""
    deadline = time.time() + 10
    sink = page.evaluate(SINK_JS)
    while sink and sink["px"] and tuple(sink["px"]) != mode and time.time() < deadline:
        time.sleep(0.2)
        sink = page.evaluate(SINK_JS)
    mark = len(obs.lines)
    page.mouse.move(x, y)
    time.sleep(0.6)
    seen = [l for l in obs.lines[mark:] if l.get("kind") in ("ptr_motion", "ptr_enter")]
    got = (round(seen[-1]["x"]), round(seen[-1]["y"])) if seen else None
    if not sink:
        return got, None
    want = ((x - sink["x"]) / sink["cssW"] * logical[0], (y - sink["y"]) / sink["cssH"] * logical[1])
    return got, (round(want[0]), round(want[1]))


def run(mode: str, scale: int = 1, desktop: str = "kde", eis: bool = True) -> "H.Results":
    gnome = desktop == "gnome"
    res = H.Results(f"wl-host-portal-{mode}" + ("-gnome" if gnome else "")
                    + (f"-scale{scale}" if scale != 1 else "") + ("" if eis else "-noeis"))
    rig = GnomeRig(scale, eis) if gnome else KdeRig(scale)
    why = rig.start()
    if why:
        rig.stop()
        H.skip_suite(f"{rig.name} portal stack unavailable: {why}")
    try:
        # The observer is the session's own client: fullscreen, so every injected pointer event
        # lands on it, and it reports the seat events it receives. Under Mutter, which shows no
        # cursor over a surface that sets none, it sets a 24-pixel one drawn at the output's
        # scale, as an application does; KWin shows its own 24-pixel cursor.
        obs = H.WlObs(rig.socket_name, XDG_RUNTIME_DIR=rig.runtime, WLOBS_FILL="ff2878dc",
                      WLOBS_CURSOR=f"24x{scale}" if gnome else "")
        res.check(f"observer client maps inside {rig.name}", obs.ready(15), H.tail(rig.log(rig.logs[0]), 3))
        mapped = next((l for l in obs.lines if l.get("kind") == "mapped"), {})
        res.check(f"{rig.name}'s output is SIZE in logical pixels at the rig's scale",
                  (mapped.get("w"), mapped.get("h")) == SIZE, mapped)
        H.server_start(mode=mode, wayland=True, extra_env={
            "SELKIES_WAYLAND_HOST_DISPLAY": rig.socket_path,
            "DBUS_SESSION_BUS_ADDRESS": rig.bus,
            "XDG_CURRENT_DESKTOP": rig.env["XDG_CURRENT_DESKTOP"],
        })
        with sync_playwright() as p:
            browser, page, console_errors, not_found = C.launch_chrome(p, mode=mode)
            try:
                res.check("the portal rung is taken for the frames",
                          C.wait_log("frames come through the xdg-desktop-portal ScreenCast", 60), H.tail(rig.log("portal"), 2))
                res.check("keyboard and pointer go through the portal",
                          C.wait_log("keyboard goes through the portal by keysym", 10)
                          and C.wait_log("pointer goes through the portal", 10), H.server_log(tail=5))
                res.check("the portal session opens with the monitor stream and both devices",
                          C.wait_log("portal session open: 1 stream(s), devices keyboard pointer", 60), H.server_log(tail=8))
                res.check("the stream negotiates the host's mode",
                          C.wait_log(f"portal stream: {rig.mode[0]}x{rig.mode[1]}", 30), H.server_log(tail=8))
                res.check("the stream reaches the streaming state", C.wait_log("portal stream streaming", 30), H.server_log(tail=8))
                res.check("no host connect failure", "connect failed" not in H.server_log(), "")

                # Input from the browser: pointer motion, a click, a wheel notch, and a key,
                # each of which the session's client must report. It goes before the wait
                # for frames: a WebRTC page that shows none reconnects 12 s after connecting,
                # and input sent meanwhile is lost.
                page.mouse.move(640, 360)
                page.mouse.click(640, 360)
                time.sleep(0.3)
                page.mouse.move(300, 200)
                time.sleep(0.3)
                page.mouse.wheel(0, 120)
                time.sleep(0.3)
                if gnome and not eis:
                    # Headless Mutter adds the seat's keyboard with the first key a session
                    # notifies, which reaches no client: one bound none yet. A desktop with a
                    # keyboard has the device already; a Control tap stands in for it.
                    page.keyboard.press("Control")
                    time.sleep(0.3)
                page.keyboard.press("a")
                time.sleep(0.3)
                page.keyboard.press("Shift+B")
                res.check("pointer motion reaches the session's client", obs.wait_for("ptr_motion", timeout=8) is not None,
                          [l for l in obs.lines if l.get("kind") == "ptr_enter"][:2])
                path = "libei" if "input via libei" in H.server_log() else "portal notify"
                if not eis:
                    res.check("with EIS refused, input goes through the portal's Notify methods",
                              path == "portal notify" and "portal offers no EIS socket" in H.server_log(),
                              H.server_log(tail=5))
                for x, y in ((900, 500), (200, 120)):
                    got, want = landing(page, obs, x, y, (mapped.get("w") or SIZE[0], mapped.get("h") or SIZE[1]),
                                        rig.mode)
                    res.check(f"the pointer lands where the page points ({x}, {y}), over {path}",
                              got is not None and want is not None
                              and abs(got[0] - want[0]) <= 2 and abs(got[1] - want[1]) <= 2,
                              f"at {got}, the page points at {want}")
                res.check("a click reaches the session's client", obs.wait_for("ptr_button", timeout=8, state=1) is not None
                          and obs.wait_for("ptr_button", timeout=8, state=0) is not None, "")
                res.check("a wheel notch reaches the session's client", obs.wait_for("ptr_axis", timeout=8) is not None, "")
                keys = [l["key"] for l in obs.lines if l.get("kind") == "kbd_key" and l.get("state") == 1]
                res.check("keys reach the session's client by keysym (a, Shift, b)",
                          obs.wait_for("kbd_key", timeout=8, key=30, state=1) is not None
                          and obs.wait_for("kbd_key", timeout=8, key=48, state=1) is not None
                          and obs.wait_for("kbd_key", timeout=8, key=42, state=1) is not None,
                          keys)
                # Past the page's one-second wheel reset, a stroke of small pixel deltas is a
                # touchpad's, which goes to the portal as smooth scroll ending in a finish.
                stroke_checks = ("a touchpad's stroke reaches the session's client unstepped, as far as the page scrolled",
                                 "and stops once the stroke pauses")
                if gnome and path == "libei":
                    # Mutter 46 through 50 hand every EIS scroll delta to the seat as a wheel's
                    # (meta-eis-client.c handle_scroll), which steps it by tens.
                    for name in stroke_checks:
                        res.skip(name, "Mutter takes libei's scroll deltas as a wheel's")
                else:
                    time.sleep(1.3)
                    if gnome:
                        # Mutter takes the pointer's focus off the client after about five
                        # seconds without motion, so the stroke follows one.
                        page.mouse.move(640, 360)
                        time.sleep(0.3)
                    mark = len(obs.lines)
                    stroke = [3.5, 6.25, 9.0, 4.75, 2.5]
                    for d in stroke:
                        page.mouse.wheel(0, d)
                        time.sleep(0.016)
                    # The finish follows the stroke's pause; give it a few seconds to cross the
                    # portal on a loaded host, and the rest of its frame.
                    time.sleep(1.0)
                    deadline = time.time() + 3.0
                    while time.time() < deadline and not any(l.get("kind") == "ptr_axis_stop"
                                                             for l in obs.lines[mark:]):
                        time.sleep(0.1)
                    time.sleep(0.2)
                    seen = [l for l in obs.lines[mark:]
                            if l.get("kind") in ("ptr_axis", "ptr_axis_source", "ptr_axis_stop")]
                    values = [l["value"] for l in seen if l["kind"] == "ptr_axis" and l.get("axis") == 0]
                    last_axis = max((i for i, l in enumerate(seen) if l["kind"] == "ptr_axis"), default=-1)
                    res.check(stroke_checks[0],
                              any(v != int(v) for v in values) and abs(sum(values) - sum(stroke)) < 0.5,
                              f"{values} against {sum(stroke)}")
                    if not gnome and "portal offers no EIS socket" in H.server_log():
                        # Without EIS the stroke goes through NotifyPointerAxis, whose finish KDE's
                        # backend drops (5.27 through master): the client is never told it stopped.
                        res.skip(stroke_checks[1], "KDE's portal takes no finish without EIS")
                    else:
                        tail = [(l["kind"][4:], l.get("axis", l.get("source")), l.get("value"))
                                for l in seen[max(last_axis, 0):]]
                        res.check(stroke_checks[1],
                                  any(l["kind"] == "ptr_axis_stop" for l in seen[last_axis + 1:]), tail)
                if C.wait_log("first portal frame", 8):
                    res.check("video reaches the browser", bool(wait_video(page, mode, 30)), "")
                else:
                    res.skip("video reaches the browser", "the compositor emitted no screencast frame")
                res.check("the portal cursor sprite is delivered when the client draws the cursor",
                          C.wait_log("portal cursor sprites arrive", 10), H.server_log(tail=4))
                # Either 24-pixel cursor comes from a buffer scale times as large.
                sprite = re.search(r"portal cursor sprites arrive \((\d+)x(\d+)", H.server_log())
                res.check("the sprite is the cursor's size at the output's scale",
                          sprite is not None and int(sprite.group(1)) == int(sprite.group(2)) == 24 * scale,
                          sprite.group(0) if sprite else "no sprite")
                real_errors, bad404 = C.benign_console(console_errors, not_found)
                res.check("no browser console errors", not real_errors and not bad404, (real_errors + bad404)[:3])
            finally:
                page.context.close()
                C.close_browser(browser)
        obs.stop()
    finally:
        H.server_stop()
        rig.stop()
    res.summary()
    return res


RUNS = {
    "websockets": dict(mode="websockets"),
    "webrtc": dict(mode="webrtc"),
    "scaled": dict(mode="websockets", scale=2),
    "gnome": dict(mode="websockets", desktop="gnome"),
    "gnome-scaled": dict(mode="websockets", scale=2, desktop="gnome"),
    "gnome-notify": dict(mode="websockets", scale=2, desktop="gnome", eis=False),
}


def main() -> bool:
    selected = [a for a in sys.argv[1:] if a in RUNS] or ["websockets"]
    ok = True
    for name in selected:
        result = run(**RUNS[name])
        ok = not result.failed() and ok
    return ok


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
