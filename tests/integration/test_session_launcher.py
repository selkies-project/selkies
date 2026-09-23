#!/usr/bin/env python3
"""selkies-session starts a whole session from one command and ends it whole.

A platform starts the launcher with a port and expects a desktop behind it.
The desktop is the host's own: a session file under the XDG data directories,
run as a display manager runs it, which here is a stand-in that records the
environment it was given and stays up. On the X11 backend the launcher starts
an Xvfb of its own that admits its cookie alone, on the Wayland backend the
session nests in Selkies' compositor, and a nested compositor from the host
(labwc) runs there as it would on a desktop. Proved with a raw WebSocket
client: the handshake completes and video frames arrive on the launcher's
port. The stock Xvfb a distribution ships runs it as well as the images'.
SIGTERM ends everything the launcher started and removes its runtime
directory, and a start script that exits leaves the session running, as a
Coder workspace's does.
"""
import asyncio
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import urllib.request
from typing import Dict, Optional, Tuple

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import websockets

SETTINGS = {"displayId": "primary", "initialClientWidth": 1280, "initialClientHeight": 720,
            "manual_resolution": False, "framerate": 60, "encoder": "jpeg", "video_crf": 25,
            "video_bitrate": 6000, "audio_bitrate": 128000, "scaling_dpi": 96, "displayPosition": "right"}
LAUNCHER = [H.PYTHON, "-m", "selkies.session"]


def status(port: int, path: str = "/api/health") -> int:
    """The HTTP status of a GET on the launcher's port, or -1 when it is not answering."""
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}{path}", timeout=2) as response:
            return response.status
    except urllib.error.HTTPError as err:
        return err.code
    except Exception:
        return -1


def data_dir(root: str, kind: str, record: str) -> str:
    """An XDG data directory holding one session of `kind`, a stand-in that
    records its environment to `record`, daemonizes a helper out of its
    process group as a desktop's agents do, and stays up."""
    folder = os.path.join(root, "share", kind)
    os.makedirs(folder, exist_ok=True)
    with open(os.path.join(folder, "standin.desktop"), "w") as fh:
        fh.write("[Desktop Entry]\nName=Stand-in\nDesktopNames=StandIn;Test;\n"
                 f"Exec=sh -c 'env > {record}; setsid sleep 3600 & exec sleep 3600'\nType=Application\n")
    return os.path.join(root, "share")


def environ(path: str, seconds: float = 30) -> Dict[str, str]:
    deadline = time.time() + seconds
    while time.time() < deadline:
        if os.path.exists(path) and os.path.getsize(path):
            time.sleep(0.2)
            return dict(line.split("=", 1) for line in open(path).read().splitlines() if "=" in line)
        time.sleep(0.2)
    return {}


def start(args: list, env: dict, log: str, detached: bool = False) -> Tuple[subprocess.Popen, int, Optional[int]]:
    """The launcher on a free port, answering. Detached, it is started in the
    background by a shell that exits at once, and its pid read back."""
    port = H._free_port()
    command = LAUNCHER + args + [f"--port={port}", "--enable-basic-auth=false"]
    pidfile = log + ".pid"
    if detached:
        quoted = " ".join("'" + a.replace("'", "'\\''") + "'" for a in command)
        command = ["sh", "-c", f"nohup {quoted} >> {log} 2>&1 & echo $! > {pidfile}"]
    proc = H.spawn(command, env=env, cwd=H.WORKDIR, stdout=open(log, "a"), stderr=subprocess.STDOUT,
                   start_new_session=True)
    deadline = time.time() + 90
    while time.time() < deadline:
        if status(port) == 200:
            pid = int(open(pidfile).read()) if detached else proc.pid
            return proc, port, pid
        if not detached and proc.poll() is not None:
            raise RuntimeError(f"the launcher exited {proc.returncode}; see {log}")
        time.sleep(0.5)
    raise RuntimeError(f"no answer on the launcher's port within 90 s; see {log}")


async def stream(port: int, seconds: float = 10.0) -> dict:
    """The data WebSocket on the launcher's port: its first message and the video frames of a primary display."""
    out = {"first": None, "frames": 0, "error": None}
    try:
        async with websockets.connect(f"ws://127.0.0.1:{port}/api/websockets", max_size=None) as ws:
            first = await asyncio.wait_for(ws.recv(), timeout=10)
            out["first"] = first if isinstance(first, str) else repr(first[:20])
            await ws.send("SETTINGS," + json.dumps(SETTINGS))
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline and out["frames"] < 30:
                try:
                    message = await asyncio.wait_for(ws.recv(), timeout=1.0)
                except asyncio.TimeoutError:
                    continue
                if isinstance(message, (bytes, bytearray)) and message and message[0] in (0x03, 0x04):
                    out["frames"] += 1
    except Exception as err:
        out["error"] = repr(err)[:120]
    return out


def leftovers(runtime: str, seconds: float = 20.0) -> list:
    """The processes still carrying the session's runtime directory once `seconds` pass, or none sooner."""
    marker = f"XDG_RUNTIME_DIR={runtime}".encode()
    deadline = time.time() + seconds
    while True:
        found = []
        for entry in os.listdir("/proc"):
            try:
                with open(f"/proc/{entry}/environ", "rb") as fh:
                    if marker in fh.read().split(b"\0"):
                        found.append((int(entry), H._cmdline(int(entry))[:60]))
            except (OSError, ValueError):
                continue
        if not found or time.time() > deadline:
            return found
        time.sleep(0.5)


def logged(log: str, prefix: str) -> Optional[str]:
    """The rest of the first launcher line starting with `prefix`."""
    for line in open(log, errors="replace"):
        if line.startswith("[selkies-session] " + prefix):
            return line[len("[selkies-session] ") + len(prefix):].strip()
    return None


def stop(proc: subprocess.Popen, pid: int, detached: bool) -> Optional[int]:
    """SIGTERM to the launcher; its exit status where this process started it."""
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        return None
    if detached:
        return None
    try:
        return proc.wait(timeout=30)
    except subprocess.TimeoutExpired:
        return None


def block(res: H.Results, tag: str, wayland: bool, session: list, path_env: Optional[str] = None,
          detached: bool = False, sound: Optional[str] = None) -> None:
    root = tempfile.mkdtemp(prefix="session-", dir=H.WORKDIR)
    record = os.path.join(root, "session.env")
    runtime = os.path.join(root, "run")
    os.makedirs(runtime, mode=0o700)
    log = os.path.join(root, "launcher.log")
    env = {"PATH": path_env or os.environ.get("PATH", ""), "HOME": os.path.expanduser("~"),
           "XDG_RUNTIME_DIR": runtime, "SELKIES_WAYLAND": "true" if wayland else "false",
           "XDG_DATA_DIRS": data_dir(root, "wayland-sessions" if wayland else "xsessions", record),
           "XDG_DATA_HOME": os.path.join(root, "home-share")}
    try:
        proc, port, pid = start(session, env, log, detached)
    except RuntimeError as err:
        res.check(f"{tag} the launcher answers on its port", False, err)
        return
    res.check(f"{tag} the launcher answers on its port", True, port)
    private = [d for d in os.listdir(runtime) if d.startswith("selkies-session-")]
    session_dir = os.path.join(runtime, private[0]) if private else runtime
    try:
        if detached:
            proc.wait(timeout=10)
            res.check(f"{tag} the launcher outlives the script that started it",
                      proc.returncode == 0 and status(port) == 200, proc.returncode)
        res.check(f"{tag} a runtime directory of the session's own", len(private) == 1, private)
        server = "pipewire" if logged(log, "pipewire serves") is not None else (
            "pulseaudio" if logged(log, "pulseaudio serves") is not None else None)
        res.check(f"{tag} a sound server of the session's own", server == sound and os.path.exists(
            os.path.join(session_dir, "pulse", "native")), (server, sound))
        if not session:
            seen = environ(record)
            res.check(f"{tag} the installed session runs, as a display manager runs it",
                      seen.get("XDG_SESSION_TYPE") == ("wayland" if wayland else "x11")
                      and seen.get("XDG_CURRENT_DESKTOP") == "StandIn:Test"
                      and seen.get("DESKTOP_SESSION") == "standin",
                      {k: seen.get(k) for k in ("XDG_SESSION_TYPE", "XDG_CURRENT_DESKTOP", "DESKTOP_SESSION")})
            if wayland:
                sockets = sorted(n for n in os.listdir(os.path.join(runtime, private[0]))
                                 if n.startswith("wayland-") and not n.endswith(".lock"))
                res.check(f"{tag} the session draws on Selkies' compositor",
                          seen.get("WAYLAND_DISPLAY") in sockets and "DISPLAY" not in seen,
                          (seen.get("WAYLAND_DISPLAY"), sockets))
            else:
                display = seen.get("DISPLAY", "")
                res.check(f"{tag} the session draws on the launcher's Xvfb, with its cookie",
                          display.startswith(":") and os.path.exists(f"/tmp/.X11-unix/X{display[1:]}")
                          and os.path.isfile(seen.get("XAUTHORITY", "")), (display, seen.get("XAUTHORITY")))
                probe = shutil.which("xdpyinfo")
                if probe:
                    anyone = subprocess.run([probe, "-display", display], env={"XAUTHORITY": os.devnull},
                                            capture_output=True).returncode
                    cookie = subprocess.run([probe, "-display", display],
                                            env={"XAUTHORITY": seen.get("XAUTHORITY", "")}, capture_output=True).returncode
                    res.check(f"{tag} the Xvfb refuses a client without the session's cookie",
                              anyone != 0 and cookie == 0, (anyone, cookie))
                else:
                    res.skip(f"{tag} the Xvfb refuses a client without the session's cookie", "no xdpyinfo")
        else:
            nested = logged(log, "desktop ")
            res.check(f"{tag} the host's compositor nests in Selkies' compositor", nested is not None, nested)
            deadline = time.time() + 20
            sockets = []
            while time.time() < deadline and len(sockets) < 2:
                sockets = [n for n in os.listdir(os.path.join(runtime, private[0]))
                           if n.startswith("wayland-") and not n.endswith(".lock")]
                time.sleep(0.5)
            res.check(f"{tag} the nested compositor offers its own socket beside Selkies'", len(sockets) >= 2, sockets)
        got = asyncio.run(stream(port))
        res.check(f"{tag} the data WebSocket handshake completes on the launcher's port",
                  got["first"] is not None and got["first"].startswith("MODE"), got)
        # A still screen streams its first paint and then nothing, a few stripes
        res.check(f"{tag} video frames flow", got["frames"] >= 5, got)
    finally:
        code = stop(proc, pid, detached)
        left = leftovers(session_dir)
        for leftover, _ in left:
            try:
                os.kill(leftover, signal.SIGKILL)
            except OSError:
                pass
    if not detached:
        res.check(f"{tag} SIGTERM ends the launcher with status 0", code == 0, code)
    res.check(f"{tag} SIGTERM ends everything the launcher started", not left and status(port) == -1, left[:3])
    res.check(f"{tag} and removes its runtime directory",
              not any(d.startswith("selkies-session-") for d in os.listdir(runtime)), os.listdir(runtime))


def sound_servers() -> Tuple[Optional[str], Optional[str]]:
    """The server the launcher takes on this host, and a PATH on which a
    failing pipewire-pulse leaves it PulseAudio, where both are installed."""
    pipewire = all(shutil.which(n) for n in ("pipewire", "pipewire-pulse")) and any(
        shutil.which(n) for n in ("wireplumber", "pipewire-media-session"))
    default = "pipewire" if pipewire else ("pulseaudio" if shutil.which("pulseaudio") else None)
    if not (pipewire and shutil.which("pulseaudio")):
        return default, None
    bin_dir = tempfile.mkdtemp(prefix="no-pipewire-pulse-", dir=H.WORKDIR)
    stub = os.path.join(bin_dir, "pipewire-pulse")
    with open(stub, "w") as fh:
        fh.write("#!/bin/sh\nexit 1\n")
    os.chmod(stub, 0o755)
    return default, bin_dir + os.pathsep + os.environ.get("PATH", "")


def stock_xvfb() -> Optional[str]:
    """A PATH whose Xvfb is the distribution's own, where one is installed apart from the first on PATH."""
    stock = "/usr/bin/Xvfb"
    if not os.path.exists(stock) or os.path.realpath(shutil.which("Xvfb") or "") == os.path.realpath(stock):
        return None
    bin_dir = tempfile.mkdtemp(prefix="stock-xvfb-", dir=H.WORKDIR)
    os.symlink(stock, os.path.join(bin_dir, "Xvfb"))
    return bin_dir + os.pathsep + os.environ.get("PATH", "")


def main() -> bool:
    res = H.Results("session-launcher")
    sound, fallback = sound_servers()
    block(res, "[x11]", False, [], sound=sound)
    if fallback:
        block(res, "[x11 PulseAudio past a failing PipeWire]", False, [], path_env=fallback, sound="pulseaudio")
    else:
        res.skip("[x11 PulseAudio past a failing PipeWire]", "PipeWire and PulseAudio are not both installed")
    path = stock_xvfb()
    if path:
        block(res, "[x11 stock Xvfb]", False, [], path_env=path, sound=sound)
    else:
        res.skip("[x11 stock Xvfb]", "the Xvfb on PATH is the distribution's own")
    block(res, "[wayland]", True, [], sound=sound)
    if shutil.which("labwc"):
        block(res, "[wayland labwc]", True, ["--session", "labwc"], sound=sound)
    else:
        res.skip("[wayland labwc]", "labwc is not installed")
    block(res, "[detached]", False, [], detached=True, sound=sound)
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
