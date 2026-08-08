#!/usr/bin/env python3
"""Shared helpers for the selkies test suites: server lifecycle, HTTP probes,
and the X11 and Wayland observation used to prove that input arrived."""
import json
import os
import shutil
import struct
import subprocess
import sys
import tempfile
import time

REPO = os.environ.get(
    "SELKIES_REPO",
    os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
SRC = os.path.join(REPO, "src")
sys.path.insert(0, SRC)
TOOLS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "tools")
# The interpreter that runs the server under test. Defaults to the one running
# the tests, so a venv with selkies installed needs no further configuration.
PYTHON = os.environ.get("SELKIES_TEST_PYTHON", sys.executable)
TEST_DISPLAY = os.environ.get("E2E_DISPLAY", ":99")
PORT = int(os.environ.get("E2E_PORT", "18080"))
BASE_URL = f"http://localhost:{PORT}"

CORE_DIST = os.path.join(REPO, "addons/selkies-web-core/dist")
CLASSIC_DIST = os.path.join(REPO, "addons/selkies-dashboard/dist")
WISH_DIST = os.path.join(REPO, "addons/selkies-dashboard-wish/dist")

WORKDIR = os.environ.get("E2E_WORKDIR", os.path.join(tempfile.gettempdir(), "selkies-tests"))
os.makedirs(WORKDIR, exist_ok=True)
LOG = os.path.join(WORKDIR, "selkies-server.log")
PIDFILE = os.path.join(WORKDIR, "selkies-server.pid")


# pgrep/pkill pattern for the server under test. Bracketed so the harness's own
# command line, which carries this string verbatim, never matches itself, and
# interpreter-agnostic so a python3.12 or a venv python is caught too.
SERVER_PATTERN = "[p]ython[0-9.]* -m selkies"


def pulse_setup():
    """Create the 'output' null sink the audio capture reads through its monitor
    source, matching the deployed layout. Module ids must not collide across
    restarts, so an existing sink is left alone. Without pactl the sink has to
    exist already; only the audio checks depend on it."""
    pactl = shutil.which("pactl")
    if not pactl:
        print("note: pactl not found, leaving the audio sink as it is", flush=True)
        return
    r = subprocess.run([pactl, "list", "short", "sources"],
                       capture_output=True, text=True)
    if "output.monitor" not in r.stdout:
        subprocess.run(
            [pactl, "load-module", "module-null-sink",
             "sink_name=output", "rate=48000", "channels=2"],
            capture_output=True, timeout=10)


def server_start(mode="websockets", wayland=False, web_root=CORE_DIST,
                 extra_env=None, port=PORT, log=LOG):
    server_stop()
    pulse_setup()
    env = {
        "PATH": os.environ.get("PATH", ""),
        "HOME": os.path.expanduser("~"),
        "XDG_RUNTIME_DIR": os.environ.get("XDG_RUNTIME_DIR", WORKDIR),
        "SELKIES_PORT": str(port),
        "SELKIES_ENABLE_BASIC_AUTH": "false",
        "SELKIES_MODE": mode,
        "SELKIES_WEB_ROOT": web_root,
        "SELKIES_WAYLAND": "true" if wayland else "false",
        "SELKIES_ENABLE_HTTPS": "false",
    }
    # WebRTC works over host candidates alone on a loopback run; a TURN REST
    # endpoint is only wired in when one is offered.
    if os.environ.get("E2E_TURN_REST_URI"):
        env["SELKIES_TURN_REST_URI"] = os.environ["E2E_TURN_REST_URI"]
    if not wayland:
        env["DISPLAY"] = TEST_DISPLAY
    if extra_env:
        env.update(extra_env)
    with open(log, "w") as lf:
        lf.write("")
    proc = subprocess.Popen(
        [PYTHON, "-m", "selkies"],
        env=env, cwd=WORKDIR,
        stdout=open(log, "a"), stderr=subprocess.STDOUT,
        start_new_session=True)
    open(PIDFILE, "w").write(str(proc.pid))
    deadline = time.time() + 75
    import urllib.request
    try:
        while time.time() < deadline:
            try:
                with urllib.request.urlopen(f"{BASE_URL}/api/status", timeout=2) as r:
                    if r.status == 200:
                        return proc
            except Exception:
                pass
            if proc.poll() is not None:
                raise RuntimeError(f"selkies died during startup; see {log}")
            time.sleep(0.5)
    except RuntimeError:
        raise
    except Exception:
        pass
    subprocess.run(["pkill", "-f", SERVER_PATTERN], capture_output=True)
    raise RuntimeError(f"selkies did not come up in time; see {log}")


def server_stop():
    """Fully stop any selkies server (two zombies must never share the port)."""
    subprocess.run(["pkill", "-f", SERVER_PATTERN], capture_output=True)
    deadline = time.time() + 10
    while time.time() < deadline:
        r = subprocess.run(["pgrep", "-f", SERVER_PATTERN], capture_output=True)
        if r.returncode != 0:
            return
        time.sleep(0.3)
    subprocess.run(["pkill", "-9", "-f", SERVER_PATTERN], capture_output=True)
    deadline = time.time() + 8
    while time.time() < deadline:
        r = subprocess.run(["pgrep", "-f", SERVER_PATTERN], capture_output=True)
        if r.returncode != 0:
            return
        time.sleep(0.3)
    raise RuntimeError("server_stop: a selkies process survived SIGKILL")


def curl(path, method="GET", data=None, headers=None, timeout=10):
    import urllib.request
    req = urllib.request.Request(BASE_URL + path, method=method)
    if headers:
        for k, v in headers.items():
            req.add_header(k, v)
    if data is not None:
        if isinstance(data, (dict, list)):
            req.data = json.dumps(data).encode()
            req.add_header("Content-Type", "application/json")
        else:
            req.data = data
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.status, r.read()


def server_log(log=LOG, tail=None):
    try:
        with open(log) as f:
            txt = f.read()
        return txt if tail is None else "\n".join(txt.splitlines()[-tail:])
    except FileNotFoundError:
        return ""


# ---------------- kernel gamepad (uinput) ----------------

UINPUT_SHIM = os.path.join(TOOLS, "uinput_shim.so")
PAD_INIT_JS = os.path.join(TOOLS, "pad_init.js")


def uinput_shim_env(tag):
    """Environment that points the server at the /dev/uinput emulator in
    tests/tools, which records the ioctls and event writes a kernel gamepad
    would receive. Returns (env, stream path, log path), both files truncated."""
    if not os.path.exists(UINPUT_SHIM):
        raise RuntimeError(f"{UINPUT_SHIM} is missing; run make -C tests/tools")
    stream = os.path.join(WORKDIR, f"uinput-{tag}-stream.bin")
    log = os.path.join(WORKDIR, f"uinput-{tag}-shim.log")
    for path in (stream, log):
        open(path, "w").close()
    env = {
        "LD_PRELOAD": UINPUT_SHIM,
        "UINPUT_SHIM_STREAM": stream,
        "UINPUT_SHIM_LOG": log,
        "SELKIES_UINPUT_GAMEPAD": "true",
    }
    return env, stream, log


def pad_init_js():
    """Browser init script installing a synthetic W3C Gamepad the tests drive."""
    with open(PAD_INIT_JS) as f:
        return f.read()


def decode_input_events(path):
    """Unpack a struct input_event stream into (type, code, value) triples."""
    with open(path, "rb") as f:
        blob = f.read()
    return [struct.unpack("=qqHHi", blob[o:o + 24])[2:]
            for o in range(0, len(blob) - 23, 24)]


# Exit code for a suite whose subject is missing from the installed
# dependencies; test_suites.py reports it as a pytest skip.
SKIP_EXIT = 77


def skip_suite(reason):
    """End the suite as skipped rather than failed. For a capability the
    installed capture stack does not expose at all, where every check would only
    report the same absence."""
    print(f"SKIP {reason}", flush=True)
    sys.exit(SKIP_EXIT)


class Results:
    def __init__(self, block):
        self.block = block
        self.items = []
        self.skipped = []

    def check(self, name, ok, detail=""):
        self.items.append((name, bool(ok), str(detail)[:160]))
        print(("PASS" if ok else "FAIL") + f"  [{self.block}] {name}  {str(detail)[:110]}", flush=True)

    def skip(self, name, reason=""):
        """Record a check the installed dependencies cannot observe. Not a
        failure: the behaviour is unproven here rather than known broken."""
        self.skipped.append((name, str(reason)[:160]))
        print(f"SKIP  [{self.block}] {name}  {str(reason)[:110]}", flush=True)

    def failed(self):
        return [i for i in self.items if not i[1]]

    def summary(self):
        f = self.failed()
        tail = f", {len(self.skipped)} skipped" if self.skipped else ""
        print(f"[{self.block}] {len(self.items) - len(f)}/{len(self.items)} passed{tail}", flush=True)
        return not f


# ---------------- X11 helpers ----------------

def x_display():
    """A selkies.Xlib Display connected to the test server."""
    from selkies.Xlib import display as xdisp
    return xdisp.Display(TEST_DISPLAY)


def x_own_clipboard(payload: bytes):
    """Own CLIPBOARD on the test X server and serve selection requests."""
    from selkies.Xlib import display as xdisp, X
    from selkies.Xlib.protocol import event as xevent
    ext = xdisp.Display(TEST_DISPLAY)
    scr = ext.screen()
    win = scr.root.create_window(0, 0, 1, 1, 0, scr.root_depth, window_class=X.InputOutput)
    clip = ext.get_atom("CLIPBOARD")
    utf8 = ext.get_atom("UTF8_STRING")
    targets = ext.get_atom("TARGETS")
    win.set_selection_owner(clip, X.CurrentTime)
    ext.flush()
    import threading
    stop = {"flag": False}

    def serve():
        # Close ext when finished serving: a leaked X11 connection keeps this
        # harness window the CLIPBOARD owner with nobody answering selection
        # requests, which wedges the NEXT block's server-side clipboard read
        # (and with it that server's whole X event queue, starving XTEST too).
        try:
            deadline = time.monotonic() + 25.0
            while not stop["flag"] and time.monotonic() < deadline:
                if ext.pending_events():
                    e = ext.next_event()
                    if isinstance(e, xevent.SelectionRequest):
                        if e.target == targets:
                            e.requestor.change_property(e.property, targets, 32, [utf8])
                        else:
                            # ChangeProperty's length field is 16-bit, so a payload
                            # larger than 64 KiB must be appended in chunks.
                            from selkies.Xlib import X as Xconst
                            off = 0
                            first = True
                            while off < len(payload) or (first and not payload):
                                chunk = payload[off:off + 60000]
                                e.requestor.change_property(
                                    e.property, e.target, 8, chunk,
                                    mode=Xconst.PropModeReplace if first else Xconst.PropModeAppend)
                                first = False
                                off += len(chunk)
                        e.requestor.send_event(xevent.SelectionNotify(
                        time=e.time, requestor=e.requestor, selection=e.selection,
                        target=e.target, property=e.property), propagate=False)
                    ext.flush()
            else:
                time.sleep(0.005)
        finally:
            try:
                ext.close()
            except Exception:
                pass

    t = threading.Thread(target=serve, daemon=True)
    t.start()
    return ext, stop


def x_read_clipboard(timeout=8.0):
    """Read current CLIPBOARD text from the test X server."""
    from selkies.Xlib import display as xdisp, X
    from selkies.Xlib.protocol import event as xevent
    d2 = xdisp.Display(TEST_DISPLAY)
    s2 = d2.screen()
    w2 = s2.root.create_window(0, 0, 1, 1, 0, s2.root_depth, window_class=X.InputOutput)
    clip2 = d2.get_atom("CLIPBOARD")
    utf82 = d2.get_atom("UTF8_STRING")
    prop = d2.get_atom("SELKIES_PROBE")
    w2.convert_selection(clip2, utf82, prop, X.CurrentTime)
    d2.flush()
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if d2.pending_events():
            e = d2.next_event()
            if isinstance(e, xevent.SelectionNotify):
                if e.property == X.NONE:
                    return None
                v = w2.get_full_property(e.property, X.AnyPropertyType)
                return bytes(v.value).decode(errors="replace") if v else None
        else:
            time.sleep(0.02)
    return None


def x_key_watcher():
    """Watch raw KeyPress/ButtonPress events on an override-redirect window on
    the test X server (a plain root event mask hits BadAccess: the streaming
    server already owns the core root mask; an override-redirect window maps
    without a WM and receives pointer/button events under the cursor)."""
    from selkies.Xlib import display as xdisp, X
    d = xdisp.Display(TEST_DISPLAY)
    scr = d.screen()
    root = scr.root
    w, h = scr.width_in_pixels, scr.height_in_pixels
    win = root.create_window(w // 2 - 100, h // 2 - 100, 200, 200, 0,
                             scr.root_depth, window_class=X.InputOutput,
                             override_redirect=True)
    win.change_attributes(event_mask=X.KeyPressMask | X.KeyReleaseMask |
                                        X.ButtonPressMask | X.ButtonReleaseMask |
                                        X.PointerMotionMask)
    win.map()
    d.set_input_focus(X.RevertToPointerRoot, X.CurrentTime, X.CurrentTime)
    d.flush()
    events = []
    stop = {"flag": False}
    import threading

    def grab():
        deadline = time.monotonic() + 30.0
        while not stop["flag"] and time.monotonic() < deadline:
            if d.pending_events():
                e = d.next_event()
                n = type(e).__name__
                if n in ("KeyPress", "KeyRelease"):
                    events.append((n, e.detail))
                elif n in ("ButtonPress", "ButtonRelease"):
                    events.append((n, e.detail))
                elif n == "MotionNotify":
                    events.append((n, (e.event_x, e.event_y)))
            else:
                time.sleep(0.01)

    t = threading.Thread(target=grab, daemon=True)
    t.start()
    return d, events, stop


def x_root_size():
    from selkies.Xlib import display as xdisp
    d = xdisp.Display(TEST_DISPLAY)
    w = d.screen().width_in_pixels
    h = d.screen().height_in_pixels
    return w, h


# ------------- Wayland helpers -------------

def _wl_env(socket_name):
    # Set through the environment rather than an `env` prefix: `env` exists even
    # where wl-clipboard does not, and would turn a missing tool into an exit
    # 127 with empty output, which reads as an empty clipboard rather than as
    # the missing dependency it is.
    return {**os.environ, "WAYLAND_DISPLAY": socket_name,
            "XDG_RUNTIME_DIR": os.environ.get("XDG_RUNTIME_DIR", WORKDIR)}


def wl_paste(socket_name, timeout=6):
    r = subprocess.run(["wl-paste", "-n"], capture_output=True, text=True,
                       timeout=timeout, env=_wl_env(socket_name))
    return r.stdout


def wl_copy(socket_name, text, timeout=10):
    """wl-copy against the compositor with one bounded retry: it can transiently
    stall on protocol dispatch when the compositor is under input load."""
    env = _wl_env(socket_name)
    for attempt in (1, 2):
        try:
            r = subprocess.run(["wl-copy", text], capture_output=True, text=True,
                               timeout=timeout, env=env)
            return r.returncode == 0
        except subprocess.TimeoutExpired:
            if attempt == 2:
                return False
            subprocess.run(["pkill", "-9", "-f", "wl-copy"], capture_output=True)
            time.sleep(1.0)
    return False


def wl_clear(socket_name, timeout=12):
    """Clear the compositor's current selection."""
    subprocess.run(
        ["env", "WAYLAND_DISPLAY=" + socket_name, "wl-copy", "-c"],
        capture_output=True, timeout=timeout,
        env={**os.environ, "XDG_RUNTIME_DIR": os.environ.get("XDG_RUNTIME_DIR", WORKDIR)})


class WlObs:
    """Drive the pywayland observer client against the compositor."""
    def __init__(self, socket_name):
        self.proc = subprocess.Popen(
            [PYTHON, os.path.join(TOOLS, "wlobs.py"), socket_name],
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
            env={**os.environ, "XDG_RUNTIME_DIR": os.environ.get("XDG_RUNTIME_DIR", WORKDIR),
                 "WLOBS_DURATION": "60"})
        self.lines = []
        self._start_reader()

    def _start_reader(self):
        import threading
        def read():
            for line in self.proc.stdout:
                line = line.strip()
                if line.startswith("{"):
                    try:
                        self.lines.append(json.loads(line))
                    except Exception:
                        pass
        threading.Thread(target=read, daemon=True).start()

    def ready(self, timeout=10):
        deadline = time.time() + timeout
        while time.time() < deadline:
            if any(l.get("kind") == "mapped" for l in self.lines):
                return True
            time.sleep(0.1)
        return False

    def wait_for(self, kind, timeout=8, **match):
        deadline = time.time() + timeout
        seen = 0
        while time.time() < deadline:
            for l in self.lines:
                if l.get("kind") != kind:
                    continue
                if all(l.get(k) == v for k, v in match.items()):
                    return l
                seen += 1
            time.sleep(0.05)
        return None

    def stop(self):
        try:
            self.proc.terminate()
        except Exception:
            pass
