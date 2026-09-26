# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""A desktop session and Selkies streaming it, from one command.

For platforms that start one process behind a port or a Unix socket: Jupyter's
server proxy, an Open OnDemand job, a Coder workspace. It starts what the host
lacks, a sound server unless one answers and an Xvfb on the X11 backend, then
the desktop and Selkies with every argument it was given. The backend is
Selkies' own `SELKIES_WAYLAND` or `--wayland`, read by Selkies' parser.

The desktop is found as a display manager finds one, among the session files
of the XDG data directories: `xsessions` on X11, `wayland-sessions` on Wayland,
where the session nests in Selkies' compositor. Nothing is assumed of the
host's Xvfb or compositor beyond what every release of them offers.
"""

import argparse
import ctypes
import glob
import os
import select
import shlex
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
from typing import Any, Callable, Dict, List, Optional, Set, Tuple

# The framebuffer bounds the sizes Selkies can resize the screen to; -s 0 -dpms
# keeps the server from blanking what is streamed.
XVFB_ARGS = ("-screen", "0", "8192x4096x24", "-nolisten", "tcp", "-noreset", "-s", "0", "-dpms")
# Where every X server puts its socket; a host's boot creates it.
X11_SOCKET_DIR = "/tmp/.X11-unix"
# What a host session leaves in the environment that must not reach this one.
HOST_SESSION_VARS = ("DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY", "DBUS_SESSION_BUS_ADDRESS",
                     "PIPEWIRE_RUNTIME_DIR", "PULSE_RUNTIME_PATH")


def log(message: str) -> None:
    print(f"[selkies-session] {message}", file=sys.stderr, flush=True)


def wait_for(predicate: Callable[[], Any], seconds: float, what: str,
             proc: Optional[subprocess.Popen] = None) -> None:
    """Poll `predicate` until it holds; raise once `proc` exits or `seconds` pass."""
    deadline = time.monotonic() + seconds
    while not predicate():
        if proc is not None and proc.poll() is not None:
            raise RuntimeError(f"{what} exited with status {proc.returncode}")
        if time.monotonic() > deadline:
            raise RuntimeError(f"{what} did not come up within {seconds:.0f}s")
        time.sleep(0.2)


def answers(path: str) -> bool:
    """Whether the Unix socket at `path` accepts a connection."""
    with socket.socket(socket.AF_UNIX) as probe:
        try:
            probe.connect(path)
            return True
        except OSError:
            return False


def x11_socket_dir(path: str = X11_SOCKET_DIR) -> None:
    """The directory X servers put their sockets in, world-writable and sticky
    as a host's boot makes it. An Xvfb creates it itself; the XWayland that
    kwin_wayland starts does not, and without it Plasma's Wayland session never
    starts its shell."""
    try:
        os.mkdir(path)
        os.chmod(path, 0o1777)  # mkdir's mode passes through the umask
    except FileExistsError:
        pass
    except OSError as err:
        log(f"cannot create {path} ({err.strerror}); the session's X11 applications will not start")


def join_group(pgid: int) -> None:
    """In a child before exec: join the session's process group, or found it."""
    try:
        os.setpgid(0, pgid)
    except OSError:  # its last member has left, or there is none yet
        os.setpgid(0, 0)


def read_entry(path: str) -> Dict[str, str]:
    """The `[Desktop Entry]` keys of a desktop file."""
    entry: Dict[str, str] = {}
    group = ""
    with open(path, encoding="utf-8", errors="replace") as fh:
        for line in fh:
            line = line.strip()
            if line.startswith("["):
                group = line
            elif group == "[Desktop Entry]" and "=" in line and not line.startswith("#"):
                key, _, value = line.partition("=")
                entry.setdefault(key.strip(), value.strip())
    return entry


def installed_sessions(wayland: bool) -> Dict[str, Dict[str, str]]:
    """The sessions a display manager would offer for the backend, by file name.

    The user's data directory comes first, so a session file there adds or
    replaces one without root; the first directory holding a name wins.
    """
    kind = "wayland-sessions" if wayland else "xsessions"
    dirs = [os.environ.get("XDG_DATA_HOME") or os.path.expanduser("~/.local/share")]
    dirs += (os.environ.get("XDG_DATA_DIRS") or "/usr/local/share:/usr/share").split(":")
    found: Dict[str, Dict[str, str]] = {}
    for folder in (os.path.join(d, kind) for d in dirs if d):
        for name in sorted(os.listdir(folder)) if os.path.isdir(folder) else ():
            if name.endswith(".desktop"):
                found.setdefault(name[:-len(".desktop")], read_entry(os.path.join(folder, name)))
    return {sid: found[sid] for sid in sorted(found) if found[sid].get("Exec") and found[sid].get("Hidden") != "true"
            and (not found[sid].get("TryExec") or shutil.which(found[sid]["TryExec"]))}


def desktop_names(entry: Dict[str, str]) -> List[str]:
    return [n for n in entry.get("DesktopNames", "").lower().replace(":", ";").split(";") if n]


def matching(sessions: Dict[str, Dict[str, str]], hints: List[str]) -> List[str]:
    """The sessions `hints` name by file, desktop, or program: one named by
    file first, then one named after its own desktop (`xfce` for XFCE, not
    `gnome-xorg` for GNOME), then name order. A command line names none."""
    keys = {os.path.basename(h).lower().removesuffix(".desktop") for h in hints if " " not in h}
    found = [sid for sid, entry in sessions.items() if sid.lower() in keys or keys.intersection(
        desktop_names(entry) + [os.path.basename(word).lower() for word in entry["Exec"].split()])]
    return sorted(found, key=lambda sid: (sid.lower() not in keys, sid.lower() not in desktop_names(sessions[sid])))


def config_value(paths: List[str], key: str) -> str:
    """What the last of `paths` to set `key` sets it to."""
    value = ""
    for path in paths:
        try:
            with open(path, encoding="utf-8", errors="replace") as fh:
                for line in fh:
                    name, sep, rest = line.partition("=")
                    if sep and name.strip() == key:
                        value = rest.strip().strip("\"'")
        except OSError:
            continue
    return value


def configured_desktops(root: str = "") -> List[str]:
    """The desktop the host starts by default, the user's last choice first:
    LightDM's and GDM's defaults, Debian's session manager alternative, and
    Red Hat's /etc/sysconfig/desktop. The system's files are read under `root`."""
    lightdm = [path for d in ("usr/share", "usr/local/share", "etc/xdg", "etc")
               for path in sorted(glob.glob(f"{root}/{d}/lightdm/lightdm.conf.d/*.conf"))]
    gdm = [f"{root}/etc/{name}" for name in ("gdm3/daemon.conf", "gdm3/custom.conf", "gdm/custom.conf")]
    manager, sysconfig = f"{root}/etc/alternatives/x-session-manager", [f"{root}/etc/sysconfig/desktop"]
    return [config_value([os.path.expanduser("~/.dmrc")], "Session"),
            config_value(lightdm + [f"{root}/etc/lightdm/lightdm.conf"], "user-session"),
            config_value(gdm, "FallbackSession"),
            os.path.realpath(manager) if os.path.islink(manager) else "",
            config_value(sysconfig, "DESKTOP"), config_value(sysconfig, "PREFERRED")]


def pick_session(wanted: Optional[str], wayland: bool) -> Tuple[str, Dict[str, str]]:
    """The session file `wanted` names by file, desktop, or program name, else
    `wanted` run as a command. Unset, the desktop XDG_CURRENT_DESKTOP names,
    then the host's default, then one named after its own desktop, then the
    first installed. A name only the other backend's sessions carry stands
    for their desktop, as `plasma` does for KDE's Wayland session on Plasma 5.
    `("", {})` when nothing is installed."""
    sessions, others = installed_sessions(wayland), installed_sessions(not wayland)
    hints = [wanted] if wanted else os.environ.get("XDG_CURRENT_DESKTOP", "").split(":") + configured_desktops()
    for hint in filter(None, hints):
        found = matching(sessions, [hint]) or matching(
            sessions, [name for sid in matching(others, [hint]) for name in desktop_names(others[sid])])
        if found:
            return found[0], sessions[found[0]]
    if wanted:
        return "", {"Exec": wanted}
    ranked = sorted(sessions, key=lambda sid: sid.lower() not in desktop_names(sessions[sid]))
    return (ranked[0], sessions[ranked[0]]) if ranked else ("", {})


def for_selkies(env: Dict[str, str]) -> Dict[str, str]:
    """Selkies' environment: the session's, with the libraries `SELKIES_PRELOAD`
    names preloaded into Selkies alone, as a packaging that carries its own
    copies needs for one the platform's drivers link against."""
    preload = os.environ.get("SELKIES_PRELOAD", "")
    if not preload:
        return env
    return dict(env, LD_PRELOAD=":".join(p for p in (preload, env.get("LD_PRELOAD", "")) if p))


def selkies_settings(args: List[str]) -> Any:
    """Selkies' settings for `args` and the environment, as its own parser reads them."""
    saved = sys.argv[1:]
    sys.argv[1:] = args
    try:
        from .settings import SETTING_DEFINITIONS, AppSettings
        return AppSettings(SETTING_DEFINITIONS)
    finally:
        sys.argv[1:] = saved


class Session:
    """The processes of one session, in a runtime directory of its own.

    The children share a process group the launcher is not in, so stopping
    ends every process they started while a signal meant for the launcher's
    caller, Ctrl-C in a terminal or a scheduler's for the job, still reaches
    the launcher and stops them in order.
    """

    def __init__(self) -> None:
        # A runtime directory an image's own init would have made is no place to start from
        base = os.environ.get("XDG_RUNTIME_DIR") or ""
        self.runtime_dir = tempfile.mkdtemp(prefix="selkies-session-",
                                            dir=base if os.path.isdir(base) and os.access(base, os.W_OK) else None)
        self.env = {k: v for k, v in os.environ.items() if k not in HOST_SESSION_VARS}
        self.env["XDG_RUNTIME_DIR"] = self.runtime_dir
        self.children: List[subprocess.Popen] = []
        self.groups: Set[int] = set()
        self.selkies: Optional[subprocess.Popen] = None

    def spawn(self, argv: List[str], env: Optional[Dict[str, str]] = None, **extra: Any) -> subprocess.Popen:
        group = next(iter(self.groups), 0)
        proc = subprocess.Popen(argv, env=env or self.env, stdin=subprocess.DEVNULL,
                                preexec_fn=lambda: join_group(group), **extra)
        try:
            self.groups.add(os.getpgid(proc.pid))
        except ProcessLookupError:
            pass
        self.children.append(proc)
        return proc

    def sound(self) -> bool:
        """A sound server Selkies can capture: the one the environment names or
        runs, else the first of PipeWire and PulseAudio that comes up here. A
        named socket that nothing serves, as an image's own init would, is passed over."""
        named = self.env.get("PULSE_SERVER", "")
        socket_path = named[len("unix:"):] if named.startswith("unix:") else named if named.startswith("/") else ""
        if named and (not socket_path or answers(socket_path)):
            return True
        running = os.environ.get("PULSE_RUNTIME_PATH") or os.path.join(os.environ.get("XDG_RUNTIME_DIR", "/nonexistent"), "pulse")
        if answers(os.path.join(running, "native")):
            self.env["PULSE_SERVER"] = f"unix:{os.path.join(running, 'native')}"
            return True
        own = os.path.join(self.runtime_dir, "pulse", "native")
        manager = next((n for n in ("wireplumber", "pipewire-media-session") if shutil.which(n)), None)
        candidates = [[["pipewire"], [manager], ["pipewire-pulse"]]] if manager else []
        candidates.append([["pulseaudio", "--daemonize=no", "--exit-idle-time=-1"]])
        for servers in candidates:
            if not all(shutil.which(argv[0]) for argv in servers):
                continue
            procs = [self.spawn(argv) for argv in servers]
            try:
                wait_for(lambda: answers(own), 20, servers[-1][0], procs[-1])
            except RuntimeError as err:
                log(str(err))
                for proc in procs:
                    proc.kill()
                continue
            self.env["PULSE_SERVER"] = f"unix:{own}"
            log(f"{servers[0][0]} serves the session's audio")
            return True
        log("no sound server came up; the session has no audio")
        return False

    def x11(self, render_dri: str) -> None:
        """An Xvfb on the first free display that admits this session's cookie
        alone, rendering on the GPU through glamor where the server offers it.

        A server whose GLX cannot load (a vendor's GL stack beside a stock
        Xvfb) is started again without it, so the desktop comes up and only
        GL applications on X11 go without.
        """
        if not shutil.which("Xvfb"):
            raise RuntimeError("the X11 backend needs Xvfb; install it, or set SELKIES_WAYLAND=true")
        auth = os.path.join(self.runtime_dir, "Xauthority")
        with open(auth, "wb") as fh:
            # One FamilyWild entry: the display number is the server's to pick
            fh.write(struct.pack(">HHHH18sH16s", 0xFFFF, 0, 0, 18, b"MIT-MAGIC-COOKIE-1", 16, os.urandom(16)))
        node = render_dri or next(iter(sorted(glob.glob("/dev/dri/renderD*"))), "")
        usage = subprocess.run(["Xvfb", "-help"], capture_output=True, text=True)
        glx = ["+extension", "GLX"]
        gpu = [glx + ["-glamor", "-dri", node]] if node and "-glamor" in usage.stdout + usage.stderr else []
        for extra in gpu + [glx, ["-extension", "GLX"]]:
            read, write = os.pipe()
            proc = self.spawn(["Xvfb", "-displayfd", str(write), "-auth", auth, *XVFB_ARGS, *extra],
                              pass_fds=(write,))
            os.close(write)
            with os.fdopen(read) as pipe:
                number = pipe.readline().strip() if select.select([pipe], [], [], 30)[0] else ""
            if number:
                self.env.update(DISPLAY=f":{number}", XAUTHORITY=auth)
                log(f"Xvfb serves display :{number}" + (f", rendering on {node}" if "-glamor" in extra else "")
                    + (", without GLX, which it could not load" if "-extension" in extra else ""))
                return
            proc.kill()
        raise RuntimeError("Xvfb did not come up")

    def capture_socket(self) -> str:
        """The socket of Selkies' compositor, the first in the runtime directory."""
        def found() -> str:
            return next((n for n in sorted(os.listdir(self.runtime_dir)) if n.startswith("wayland-")
                         and not n.endswith(".lock") and answers(os.path.join(self.runtime_dir, n))), "")
        wait_for(found, 60, "Selkies' compositor", self.selkies)
        return found()

    def desktop(self, sid: str, entry: Dict[str, str], wayland: bool) -> None:
        """The session as a display manager starts one, on a session bus of its own."""
        env = dict(self.env, XDG_SESSION_TYPE="wayland" if wayland else "x11")
        if wayland:
            env["WAYLAND_DISPLAY"] = self.capture_socket()
            x11_socket_dir()
        names = entry.get("DesktopNames", "").strip(";").replace(";", ":")
        if names:
            env["XDG_CURRENT_DESKTOP"] = names
        else:
            env.pop("XDG_CURRENT_DESKTOP", None)
        if sid:
            env.update(XDG_SESSION_DESKTOP=sid, DESKTOP_SESSION=sid)
        # The interposers serve the session's applications; Selkies keeps the real device nodes
        preload = [env.get(v, "") for v in ("SELKIES_INTERPOSER", "SELKIES_WEBCAM_INTERPOSER")]
        preload = [p for p in preload if os.path.isfile(p)] + [p for p in env.get("LD_PRELOAD", "").split(":") if p]
        if preload:
            env["LD_PRELOAD"] = ":".join(preload)
        bus = ["dbus-run-session", "--"] if shutil.which("dbus-run-session") else []
        self.spawn(bus + shlex.split(entry["Exec"]), env=env)
        log(f"desktop {sid or entry['Exec']} on {env.get('WAYLAND_DISPLAY') or env['DISPLAY']}")

    def stragglers(self) -> List[int]:
        """The processes still carrying the session's runtime directory, such
        as the agents a desktop daemonizes out of its process group."""
        marker = f"XDG_RUNTIME_DIR={self.runtime_dir}".encode()
        found = []
        for entry in filter(str.isdigit, os.listdir("/proc")):
            try:
                with open(f"/proc/{entry}/environ", "rb") as fh:
                    if marker in fh.read().split(b"\0"):
                        found.append(int(entry))
            except OSError:
                continue
        return found

    def signal_groups(self, sig: int) -> None:
        for pgid in self.groups:
            try:
                os.killpg(pgid, sig)
            except OSError:
                pass

    def stop(self) -> None:
        """Selkies first, so its capture closes before its display, then the
        session's process groups and whatever left them, and the runtime directory."""
        if self.selkies is not None and self.selkies.poll() is None:
            self.selkies.terminate()
            try:
                self.selkies.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.selkies.kill()
        self.signal_groups(signal.SIGTERM)
        deadline = time.monotonic() + 5
        for proc in self.children:
            try:
                proc.wait(timeout=max(0.1, deadline - time.monotonic()))
            except subprocess.TimeoutExpired:
                pass
        self.signal_groups(signal.SIGKILL)
        for pid in self.stragglers():
            try:
                os.kill(pid, signal.SIGKILL)
            except OSError:
                pass
        for proc in self.children:
            proc.wait()
        shutil.rmtree(self.runtime_dir, ignore_errors=True)


def follow_parent() -> None:
    """Have the kernel send SIGTERM when the parent exits."""
    try:
        ctypes.CDLL(None, use_errno=True).prctl(1, signal.SIGTERM)  # PR_SET_PDEATHSIG
    except (AttributeError, OSError):
        pass


def parse(argv: List[str]) -> Tuple[argparse.Namespace, List[str]]:
    parser = argparse.ArgumentParser(
        prog="selkies-session", allow_abbrev=False,
        description="Start a desktop session and Selkies streaming it; every other argument goes to selkies.")
    parser.add_argument("--session", help="a session by file, desktop, or program name (plasma, KDE, xfce, "
                                          "startlxqt), else a command to run; unset, the desktop "
                                          "XDG_CURRENT_DESKTOP names, else the host's default desktop")
    opts, rest = parser.parse_known_args(argv)
    return opts, rest[1:] if rest[:1] == ["--"] else rest


def main(argv: Optional[List[str]] = None, follow: bool = False) -> int:
    """Run a session until Selkies ends or a signal arrives.

    Args:
        argv: The launcher's arguments; `sys.argv[1:]` when None.
        follow: End the session when the parent exits, for a parent that owns
            it and may die without stopping it. Off for the console script,
            which a start script may leave running in the background.

    Returns:
        Selkies' exit status, or 0 after a signal.
    """
    opts, selkies_args = parse(sys.argv[1:] if argv is None else argv)
    settings = selkies_settings(selkies_args)
    wayland = bool(settings.wayland[0])
    if follow:
        follow_parent()
    stopping: List[int] = []
    for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        if signal.getsignal(sig) is not signal.SIG_IGN:  # nohup's, and a background job's SIGINT
            signal.signal(sig, lambda signum, _: stopping.append(signum))
    session = Session()
    status = 1
    try:
        command = [sys.executable, "-m", "selkies", *([] if session.sound() else ["--audio-enabled=false"]),
                   *selkies_args]
        if not wayland:
            session.x11(str(settings.render_dri or ""))
        sid, entry = pick_session(opts.session, wayland)
        if wayland:
            session.selkies = session.spawn(command, env=for_selkies(session.env))
        if entry:
            session.desktop(sid, entry, wayland)
        else:
            log(f"no {'Wayland' if wayland else 'X11'} session is installed; the display stays empty")
        if not wayland:
            session.selkies = session.spawn(command, env=for_selkies(session.env))
        while session.selkies.poll() is None and not stopping:
            time.sleep(0.5)
        status = 0 if stopping else session.selkies.returncode
    except RuntimeError as err:
        log(str(err))
    finally:
        session.stop()
    return status


def jupyter() -> Dict[str, Any]:
    """The server process jupyter-server-proxy starts a desktop with.

    Jupyter authenticates the route and the proxy's Unix socket sits in a
    directory only this user reaches, so Selkies runs without its own login
    or TLS.
    The session follows the server down, which on a kill stops nothing itself.
    A notebook panel cannot hold the pointer or the keyboard, hence a tab.
    """
    return {
        "command": [sys.executable, "-c", "import sys, selkies.session as s; sys.exit(s.main(follow=True))",
                    "--unix-socket", "{unix_socket}", "--enable-basic-auth=false", "--enable-https=false"],
        "unix_socket": True,
        "timeout": 120,
        "new_browser_tab": True,
        "launcher_entry": {
            "title": "Selkies",
            "icon_path": os.path.join(os.path.dirname(os.path.abspath(__file__)), "selkies_web", "selkies.svg"),
        },
    }


if __name__ == "__main__":
    sys.exit(main())
