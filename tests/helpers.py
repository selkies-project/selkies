#!/usr/bin/env python3
"""Shared helpers for the selkies test suites: server lifecycle, HTTP probes,
and the X11 and Wayland observation used to prove that input arrived."""
import ctypes
import json
import os
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
from typing import Any, Iterable, NoReturn, Optional

REPO = os.environ.get(
    "SELKIES_REPO",
    os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
SRC = os.path.join(REPO, "src")
sys.path.insert(0, SRC)
TOOLS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "tools")
# The interpreter that runs the server under test. Defaults to the one running
# the tests, so a venv with selkies installed needs no further configuration.
PYTHON = os.environ.get("SELKIES_TEST_PYTHON", sys.executable)
# Tools beside that interpreter (ffmpeg/ffplay in a conda env) are findable
# without activation; appended, so a system tool of the same name still wins.
_PY_BIN = os.path.dirname(os.path.abspath(PYTHON))
if _PY_BIN not in os.environ.get("PATH", "").split(os.pathsep):
    os.environ["PATH"] = os.environ.get("PATH", "") + os.pathsep + _PY_BIN
# Never inherited from DISPLAY: the suites resize the root and inject input,
# which must not land on a session in use. private_x_server() needs nothing set.
TEST_DISPLAY = os.environ.get("E2E_DISPLAY", "")


def _free_port() -> int:
    """A loopback port nothing is listening on, from the kernel's own pool.

    Asked for rather than assumed, for the reason the display number is: a
    fixed one makes two runs on a host collide, and the second reads the
    first's server as its own.
    """
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


# Each suite process gets its own, so runs do not have to be serialised; naming
# one pins it (a proxy or firewall rule in front of the server needs that).
PORT = int(os.environ.get("E2E_PORT") or _free_port())
BASE_URL = f"http://localhost:{PORT}"

CORE_DIST = os.path.join(REPO, "addons/selkies-web-core/dist")
CLASSIC_DIST = os.path.join(REPO, "addons/selkies-dashboard/dist")
WISH_DIST = os.path.join(REPO, "addons/selkies-dashboard-wish/dist")

WORKDIR = os.environ.get("E2E_WORKDIR", os.path.join(tempfile.gettempdir(), "selkies-tests"))
os.makedirs(WORKDIR, exist_ok=True)
LOG = os.path.join(WORKDIR, "selkies-server.log")
PIDFILE = os.path.join(WORKDIR, "selkies-server.pid")


def _cmdline(pid: int) -> str:
    """Space-joined command line of `pid`, or "" if it is gone."""
    try:
        with open(f"/proc/{pid}/cmdline", "rb") as f:
            return f.read().replace(b"\0", b" ").decode("utf-8", "replace")
    except OSError:
        return ""


def _listener_pids(port: int) -> set:
    """PIDs listening on `port`, read from /proc so no iproute2 is needed."""
    inodes = set()
    for path in ("/proc/net/tcp", "/proc/net/tcp6"):
        try:
            with open(path) as f:
                lines = f.read().splitlines()[1:]
        except OSError:
            continue
        for line in lines:
            fields = line.split()
            # 0A is TCP_LISTEN in the /proc/net/tcp state column.
            if len(fields) < 10 or fields[3] != "0A":
                continue
            try:
                if int(fields[1].rsplit(":", 1)[1], 16) == port:
                    inodes.add(fields[9])
            except (IndexError, ValueError):
                continue
    if not inodes:
        return set()
    pids = set()
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        fddir = f"/proc/{entry}/fd"
        try:
            for fd in os.listdir(fddir):
                try:
                    target = os.readlink(os.path.join(fddir, fd))
                except OSError:
                    continue
                if target.startswith("socket:[") and target[8:-1] in inodes:
                    pids.add(int(entry))
                    break
        except OSError:
            continue
    return pids


def server_pids(port: int = PORT) -> set:
    """The servers this harness is responsible for: the one it started, plus
    whatever selkies is listening on the test port (a previous run that died
    without cleaning up). Deliberately not every `python -m selkies` on the
    machine — a real session streaming the host's own desktop is not the
    harness's to kill, and sweeping by name has ended one."""
    pids = set()
    try:
        with open(PIDFILE) as f:
            recorded = int(f.read().strip())
        if "selkies" in _cmdline(recorded):
            pids.add(recorded)
    except (OSError, ValueError):
        pass
    for pid in _listener_pids(port):
        if "selkies" in _cmdline(pid):
            pids.add(pid)
    return pids


def pulse_setup() -> None:
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
    # Reset on every setup: a suite that pointed the default elsewhere and died
    # would otherwise leave every later audio check reading silence.
    subprocess.run([pactl, "set-default-sink", "output"],
                   capture_output=True, timeout=10)


def pulse_null_sink(name: str, **opts: Any) -> Optional[str]:
    """Load a null sink named `name`, returning the module id to unload later.

    Nothing is loaded when a sink of that name already exists, and None comes
    back: pactl would otherwise happily create a second one under the same
    name, which is how a host ends up with six of them. Pass the id to
    pulse_unload() when finished — a sink left behind outlives the run that
    made it and every later run inherits it.
    """
    pactl = shutil.which("pactl")
    if not pactl:
        return None
    existing = subprocess.run([pactl, "list", "short", "sinks"],
                              capture_output=True, text=True).stdout
    if any(line.split("\t")[1:2] == [name] for line in existing.splitlines()):
        return None
    args = [f"sink_name={name}"] + [f"{k}={v}" for k, v in opts.items()]
    r = subprocess.run([pactl, "load-module", "module-null-sink"] + args,
                       capture_output=True, text=True, timeout=10)
    module_id = r.stdout.strip()
    return module_id if module_id.isdigit() else None


def pulse_unload(module_id: Optional[str]) -> None:
    """Unload a module pulse_null_sink() loaded; a no-op for None."""
    pactl = shutil.which("pactl")
    if pactl and module_id:
        subprocess.run([pactl, "unload-module", module_id],
                       capture_output=True, timeout=10)


PR_SET_PDEATHSIG = 1
_LIBC = ctypes.CDLL(None, use_errno=True)


def _die_with_parent() -> None:
    """preexec for spawned children: the kernel SIGKILLs the child the moment
    the test process dies, so an externally killed run (timeout or SIGKILL
    skips every ``finally``) cannot orphan servers or damage feeds into the
    shared display and port fixtures. The reparent check closes the
    fork-to-prctl race."""
    _LIBC.prctl(PR_SET_PDEATHSIG, signal.SIGKILL)
    if os.getppid() == 1:
        os._exit(1)


def spawn(cmd: Iterable, **kwargs: Any) -> subprocess.Popen:
    """subprocess.Popen for a test child that must not outlive the suite."""
    kwargs.setdefault("preexec_fn", _die_with_parent)
    return subprocess.Popen(cmd, **kwargs)


def server_start(mode: str = "websockets", wayland: bool = False,
                 web_root: str = CORE_DIST,
                 extra_env: Optional[dict] = None,
                 port: int = PORT, log: str = LOG) -> subprocess.Popen:
    """Start a selkies server and wait until /api/status answers.

    Any previous server on the port is stopped first; the environment is built
    from scratch rather than inherited, so a variable set in the shell running
    the tests cannot silently reconfigure the server under test.

    Args:
        mode: Transport, "websockets" or "webrtc".
        wayland: Capture backend; False runs against the X test display.
        web_root: Directory served as the web client.
        extra_env: Extra environment variables merged over the base set.
        port: HTTP port the server listens on.
        log: File that receives the server's combined stdout/stderr.

    Returns:
        The server process, already answering on `/api/status`.

    Raises:
        RuntimeError: The server exited during startup or never came up.
    """
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
    # Warnings config crosses into the server so a deprecation sweep can see
    # server-side hits; everything else stays hermetic.
    if os.environ.get("PYTHONWARNINGS"):
        env["PYTHONWARNINGS"] = os.environ["PYTHONWARNINGS"]
    if not wayland:
        env["DISPLAY"] = require_display()
    if extra_env:
        env.update(extra_env)
    with open(log, "w") as lf:
        lf.write("")
    proc = spawn(
        [PYTHON, "-m", "selkies"],
        env=env, cwd=WORKDIR,
        stdout=open(log, "a"), stderr=subprocess.STDOUT,
        start_new_session=True)
    open(PIDFILE, "w").write(str(proc.pid))
    deadline = time.time() + 75
    import urllib.request
    # A subfolder deployment moves every route, this probe included.
    prefix = env.get("SELKIES_SUBFOLDER", "").strip("/")
    status_url = f"{BASE_URL}/{prefix}/api/status" if prefix else f"{BASE_URL}/api/status"
    try:
        while time.time() < deadline:
            try:
                with urllib.request.urlopen(status_url, timeout=2) as r:
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
    server_stop(port=port)
    raise RuntimeError(f"selkies did not come up in time; see {log}")


def _signal(pids: Iterable[int], sig: int) -> None:
    for pid in pids:
        try:
            os.kill(pid, sig)
        except OSError:
            pass


def server_stop(port: int = PORT) -> None:
    """Fully stop the server under test (two zombies must never share the port)."""
    _signal(server_pids(port), signal.SIGTERM)
    deadline = time.time() + 10
    while time.time() < deadline:
        if not server_pids(port):
            return
        time.sleep(0.3)
    _signal(server_pids(port), signal.SIGKILL)
    deadline = time.time() + 8
    while time.time() < deadline:
        if not server_pids(port):
            return
        time.sleep(0.3)
    raise RuntimeError("server_stop: a selkies process survived SIGKILL")


def curl(path, method="GET", data=None, headers=None, timeout=10) -> tuple:
    """HTTP request against the test server.

    Args:
        path: URL path, joined onto BASE_URL.
        method: HTTP method.
        data: Request body; a dict or list is sent as JSON, bytes as-is.
        headers: Extra request headers.
        timeout: Socket timeout in seconds.

    Returns:
        `(status, body_bytes)`.
    """
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


def server_log(log: str = LOG, tail: Optional[int] = None) -> str:
    """The server log, optionally only its last `tail` lines."""
    try:
        with open(log) as f:
            txt = f.read()
        return txt if tail is None else "\n".join(txt.splitlines()[-tail:])
    except FileNotFoundError:
        return ""


UINPUT_SHIM = os.path.join(TOOLS, "uinput_shim.so")
PAD_INIT_JS = os.path.join(TOOLS, "pad_init.js")


def uinput_shim_env(tag: str) -> tuple:
    """Environment that points the server at the /dev/uinput emulator in
    tests/tools, which records the ioctls and event writes a kernel gamepad
    would receive.

    Args:
        tag: Distinguishes this run's recording files in the workdir.

    Returns:
        `(env, stream_path, log_path)`, both files truncated.

    Raises:
        RuntimeError: The shim library has not been built.
    """
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


def pad_init_js() -> str:
    """Browser init script installing a synthetic W3C Gamepad the tests drive."""
    with open(PAD_INIT_JS) as f:
        return f.read()


def decode_input_events(path: str) -> list:
    """Unpack a struct input_event stream into (type, code, value) triples."""
    with open(path, "rb") as f:
        blob = f.read()
    return [struct.unpack("=qqHHi", blob[o:o + 24])[2:]
            for o in range(0, len(blob) - 23, 24)]


# Exit code for a suite whose subject is missing from the installed
# dependencies; test_suites.py reports it as a pytest skip.
SKIP_EXIT: int = 77


def skip_suite(reason: str) -> NoReturn:
    """End the suite as skipped rather than failed. For a capability the
    installed capture stack does not expose at all, where every check would only
    report the same absence."""
    print(f"SKIP {reason}", flush=True)
    sys.exit(SKIP_EXIT)


class Results:
    """Collects PASS/FAIL/SKIP check lines for one suite block and prints each
    as it is recorded, in the line format test_suites.py parses."""

    def __init__(self, block: str) -> None:
        self.block = block
        self.items: list = []
        self.skipped: list = []

    def check(self, name: str, ok: Any, detail: Any = "") -> None:
        """Record one check; any truthy `ok` passes."""
        self.items.append((name, bool(ok), str(detail)[:160]))
        print(("PASS" if ok else "FAIL") + f"  [{self.block}] {name}  {str(detail)[:110]}", flush=True)

    def skip(self, name: str, reason: Any = "") -> None:
        """Record a check the installed dependencies cannot observe. Not a
        failure: the behaviour is unproven here rather than known broken."""
        self.skipped.append((name, str(reason)[:160]))
        print(f"SKIP  [{self.block}] {name}  {str(reason)[:110]}", flush=True)

    def failed(self) -> list:
        return [i for i in self.items if not i[1]]

    def summary(self) -> bool:
        """Print the block's pass count; True when every check passed."""
        f = self.failed()
        tail = f", {len(self.skipped)} skipped" if self.skipped else ""
        print(f"[{self.block}] {len(self.items) - len(f)}/{len(self.items)} passed{tail}", flush=True)
        return not f


def require_display() -> str:
    """The X display the suites drive, or a failure that says what to set."""
    if not TEST_DISPLAY:
        raise RuntimeError(
            "E2E_DISPLAY is not set: point it at a throwaway X server for the "
            "suites to drive (see tests/README.md). DISPLAY is not inherited, "
            "because these suites resize the root and inject input.")
    return TEST_DISPLAY


def free_display(taken: Iterable[str] = ()) -> Iterable[str]:
    """Display numbers no X server on this host holds, most likely first.

    Both the lock file and the socket are consulted, since a server that died
    without cleaning up leaves one or the other behind. The numbers start well
    above those desktop sessions and container entrypoints take, so a throwaway
    server never lands on a display someone is working on. Yields candidates
    rather than one answer: only the server that wins the bind knows for sure.
    """
    skip = {d.lstrip(":").split(".")[0] for d in taken if d}
    for number in range(64, 256):
        if str(number) in skip:
            continue
        if os.path.exists(f"/tmp/.X{number}-lock"):
            continue
        if os.path.exists(f"/tmp/.X11-unix/X{number}"):
            continue
        yield f":{number}"


def private_x_server(width: int = 1280, height: int = 720, depth: int = 24,
                     extra_args: Iterable[str] = ()) -> tuple:
    """A throwaway Xvfb of this suite's own, on a display nothing else holds.

    GLX is off because it faults on some GPU hosts and no suite that wants a
    private server needs it. A number another server takes first is simply the
    next candidate, so suites running side by side do not collide; the attempts
    are bounded, because a host where no server can start at all must report
    that rather than work through every number in the range.

    Args:
        width: Screen width. Xvfb fixes its maximum screen size here, so a
            suite that wants a server which refuses to grow asks for a small one.
        height: Screen height.
        depth: Colour depth.
        extra_args: Further Xvfb arguments, appended.

    Returns:
        `(process, display)`; the caller terminates the process.

    Raises:
        RuntimeError: Xvfb or xdpyinfo is missing, or no candidate came up.
    """
    for tool in ("Xvfb", "xdpyinfo"):
        if not shutil.which(tool):
            raise RuntimeError(f"{tool} is not installed; a private X server needs it")
    candidates = free_display(taken=(TEST_DISPLAY, os.environ.get("DISPLAY", "")))
    for _, display in zip(range(8), candidates):
        proc = spawn(
            ["Xvfb", display, "-screen", "0", f"{width}x{height}x{depth}",
             "-extension", "GLX", "-nolisten", "tcp", "-ac", "-noreset",
             *extra_args],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            start_new_session=True)
        deadline = time.time() + 15
        while time.time() < deadline:
            if proc.poll() is not None:
                break
            if subprocess.run(["xdpyinfo", "-display", display],
                              capture_output=True).returncode == 0:
                return proc, display
            time.sleep(0.25)
        proc.kill()
        proc.wait(timeout=5)
    raise RuntimeError("no display number yielded a working X server")


def stop_x_server(proc: subprocess.Popen, display: str) -> None:
    """Stop a private_x_server() and leave its display number reusable.

    Asked to quit rather than killed outright: a killed X server never removes
    its lock file or socket, and free_display() then treats that number as
    taken for as long as the machine is up. Anything the server did leave
    behind is cleaned here, but only when the lock names the process that has
    just died, so a number already reissued to someone else is untouched.
    """
    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5)
    number = display.lstrip(":").split(".")[0]
    lock = f"/tmp/.X{number}-lock"
    try:
        with open(lock) as f:
            stale = int(f.read().strip()) == proc.pid
    except (OSError, ValueError):
        return
    if not stale:
        return
    for path in (lock, f"/tmp/.X11-unix/X{number}"):
        try:
            os.unlink(path)
        except OSError:
            pass


def x_display() -> Any:
    """A selkies.Xlib Display connected to the test server."""
    from selkies.Xlib import display as xdisp
    return xdisp.Display(require_display())


def x_own_clipboard(payload: bytes) -> tuple:
    """Own CLIPBOARD on the test X server and serve selection requests.

    Args:
        payload: Bytes handed to any requestor asking for the selection.

    Returns:
        `(display, stop)` where setting `stop["flag"]` ends the serving thread.
    """
    from selkies.Xlib import display as xdisp, X
    from selkies.Xlib.protocol import event as xevent
    ext = xdisp.Display(require_display())
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
        # ext must be closed when done: a leaked connection leaves this window
        # the CLIPBOARD owner with nobody answering, wedging the next block's
        # server-side clipboard read and with it that server's X event queue.
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


def x_read_clipboard(timeout: float = 8.0) -> Optional[str]:
    """Read current CLIPBOARD text from the test X server."""
    from selkies.Xlib import display as xdisp, X
    from selkies.Xlib.protocol import event as xevent
    d2 = xdisp.Display(require_display())
    # Closed on every path: an X server allows a bounded number of clients, and
    # a helper called once per clipboard assertion exhausts it part-way through a
    # full run, which surfaces as unrelated suites failing to reach the display.
    try:
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
    finally:
        d2.close()


def x_key_watcher() -> tuple:
    """Watch raw KeyPress/ButtonPress events on an override-redirect window on
    the test X server (a plain root event mask hits BadAccess: the streaming
    server already owns the core root mask; an override-redirect window maps
    without a WM and receives pointer/button events under the cursor).

    Returns:
        `(display, events, stop)`: the events list fills from a daemon thread
        until `stop["flag"]` is set or 30 seconds pass.
    """
    from selkies.Xlib import display as xdisp, X
    d = xdisp.Display(require_display())
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


def x_root_size() -> tuple:
    """Current root dimensions, read on a fresh connection.

    The size rides the connection handshake, so a new one is what picks up a
    RandR resize; it is closed here because callers poll this in a loop.
    """
    from selkies.Xlib import display as xdisp
    d = xdisp.Display(require_display())
    try:
        return d.screen().width_in_pixels, d.screen().height_in_pixels
    finally:
        d.close()


def _wl_env(socket_name: str) -> dict:
    """Environment for a wl-clipboard call against `socket_name`.

    Set through the environment rather than an `env` prefix: `env` exists even
    where wl-clipboard does not, and would turn a missing tool into an exit
    127 with empty output, which reads as an empty clipboard rather than as
    the missing dependency it is.
    """
    return {**os.environ, "WAYLAND_DISPLAY": socket_name,
            "XDG_RUNTIME_DIR": os.environ.get("XDG_RUNTIME_DIR", WORKDIR)}


def wl_paste(socket_name: str, timeout: float = 6) -> str:
    """Current selection text on the compositor at `socket_name`."""
    r = subprocess.run(["wl-paste", "-n"], capture_output=True, text=True,
                       timeout=timeout, env=_wl_env(socket_name))
    return r.stdout


def wl_copy(socket_name: str, text: str, timeout: float = 10) -> bool:
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


def wl_clear(socket_name: str, timeout: float = 12) -> None:
    """Clear the compositor's current selection."""
    subprocess.run(
        ["env", "WAYLAND_DISPLAY=" + socket_name, "wl-copy", "-c"],
        capture_output=True, timeout=timeout,
        env={**os.environ, "XDG_RUNTIME_DIR": os.environ.get("XDG_RUNTIME_DIR", WORKDIR)})


class WlObs:
    """Drive the pywayland observer client (tests/tools/wlobs.py) against the
    compositor and collect its JSONL event lines from a reader thread."""

    def __init__(self, socket_name: str) -> None:
        self.proc = spawn(
            [PYTHON, os.path.join(TOOLS, "wlobs.py"), socket_name],
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
            env={**os.environ, "XDG_RUNTIME_DIR": os.environ.get("XDG_RUNTIME_DIR", WORKDIR),
                 "WLOBS_DURATION": "60"})
        self.lines: list = []
        self._start_reader()

    def _start_reader(self) -> None:
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

    def ready(self, timeout: float = 10) -> bool:
        """True once the observer surface is mapped and receiving events."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            if any(l.get("kind") == "mapped" for l in self.lines):
                return True
            time.sleep(0.1)
        return False

    def wait_for(self, kind: str, timeout: float = 8, **match: Any) -> Optional[dict]:
        """First event of `kind` whose fields equal `match`, or None on timeout."""
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

    def stop(self) -> None:
        try:
            self.proc.terminate()
        except Exception:
            pass
