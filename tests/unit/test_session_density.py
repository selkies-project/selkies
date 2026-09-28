#!/usr/bin/env python3
"""What the density ladder reads about the session, and what it remembers.

A desktop that keeps its settings on a session bus is reached through the
environment its own process runs with, which is read out of /proc rather than
inherited, and the density the ladder last applied is what a page joining an
existing session is told the desktop already has. The readers run against real
processes.
"""

import asyncio
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, TESTS)
sys.path.insert(0, os.path.join(os.path.dirname(TESTS), "src"))

import helpers as H  # noqa: E402
from selkies import display_utils as DU  # noqa: E402

res = H.Results("session-density")

res.check("every local form of a display name reaches one server, and a TCP name only its own host's",
          [DU._display_server(n) for n in (":20", ":20.0", "unix:20", "unix/:20", "localhost:20",
                                           "tcp/localhost:20", ":20x", "", None)]
          == [("", "20")] * 4 + [("localhost", "20")] * 2 + [None] * 3)
sleepers = {d: subprocess.Popen(["sleep", "30"], env={"PATH": os.environ.get("PATH", ""),
                                                      "SELKIES_PROBE": "density", "DISPLAY": d})
            for d in (":97", "localhost:98", ":98.0", "unix/:98")}
suite_display = os.environ.get("DISPLAY")
try:
    time.sleep(0.2)
    env = DU._process_environ(sleepers[":98.0"].pid)
    res.check("a process's environment is read back from /proc",
              env.get("SELKIES_PROBE") == "density", str(env))
    os.environ["DISPLAY"] = "unix:98"
    pids = asyncio.run(DU._pids_on_display("sleep"))
    found = sorted(d for d, p in sleepers.items() if p.pid in pids)
    res.check("the processes running a binary are found on this display alone, however it is written",
              found == [":98.0", "unix/:98"], found)
    res.check("a binary nothing is running has no pids",
              asyncio.run(DU._pids_on_display("selkies-no-such-binary")) == [])
finally:
    if suite_display is None:
        os.environ.pop("DISPLAY", None)
    else:
        os.environ["DISPLAY"] = suite_display
    for sleeper in sleepers.values():
        sleeper.kill()
        sleeper.wait()

res.check("a process that is gone has no environment to read",
          DU._process_environ(sleepers[":98.0"].pid) == {})

res.check("no density is reported before one is applied", DU.applied_dpi() is None)
DU._APPLIED_DPI = 192
res.check("the density last applied is what a joining page is told", DU.applied_dpi() == 192)
DU._APPLIED_DPI = None

# The density a restart finds in the home, and what startup makes of it.
home = tempfile.mkdtemp(prefix="density-home-")
saved_env = {k: os.environ.get(k) for k in ("HOME", "DISPLAY", "XDG_CONFIG_HOME")}
os.environ["HOME"] = home
os.environ["XDG_CONFIG_HOME"] = os.path.join(home, ".config")
os.environ.pop("DISPLAY", None)
xserver = display = other_server = None
try:
    res.check("a fresh home names no density", DU.desktop_dpi() is None, DU.desktop_dpi())
    with open(os.path.join(home, ".xsettingsd"), "w") as f:
        f.write("Xft/Antialias 1\nXft/DPI 147456\n")
    res.check("persisted xsettingsd resources name the density they serve",
              DU.desktop_dpi() == 144, DU.desktop_dpi())
    with open(os.path.join(home, ".Xresources"), "w") as f:
        f.write("*background: black\nXft.dpi:   192\n")
    res.check("persisted Xresources are read over xsettingsd",
              DU.desktop_dpi() == 192, DU.desktop_dpi())
    try:
        xserver, display = H.private_x_server(320, 240)
    except RuntimeError as e:
        res.skip("the server's resource database is read over the files", e)
    else:
        os.environ["DISPLAY"] = display
        res.check("a resource database without Xft.dpi falls back to the files",
                  DU.desktop_dpi() == 192, DU.desktop_dpi())
        subprocess.run(["xrdb", "-merge", "-"], input="Xft.dpi: 120\n", text=True,
                       env=dict(os.environ), check=True)
        res.check("the server's resource database is read over the files",
                  DU.desktop_dpi() == 120, DU.desktop_dpi())

        # Stand-ins named xsettingsd, one serving this display and one another
        # display from this same home, which a SIGHUP ends. The other display is
        # a server of this suite's own: the readers find a process by its DISPLAY
        # across the host, so a number nothing holds can be a concurrent run's.
        other_server, other = H.private_x_server(320, 240)
        daemons = {d: H.named_process("xsettingsd", {"DISPLAY": d}) for d in (display, other)}
        try:
            merged = asyncio.run(DU._run_xrdb(144, DU.logger_app_resize))
            time.sleep(0.3)
            with open(os.path.join(home, ".xsettingsd")) as f:
                served = f.read()
            res.check("a density reaches the resource database and the xsettingsd file",
                      merged and DU.desktop_dpi() == 144 and "Xft/DPI 147456" in served, served)
            res.check("only the xsettingsd serving this display is told to reload",
                      daemons[display].poll() == -signal.SIGHUP and daemons[other].poll() is None,
                      {d: p.poll() for d, p in daemons.items()})
        finally:
            for daemon in daemons.values():
                daemon.kill()
                daemon.wait()
            subprocess.run(["xrdb", "-merge", "-"], input="Xft.dpi: 120\n", text=True,
                           env=dict(os.environ), check=True)
        lxqt = os.path.join(home, "lxqt.conf")
        with open(lxqt, "w") as f:
            f.write('[General]\nicon_theme=x\n[Qt]\nfont="Sans,-1,20,5,50,0,0,0,0,0"\n')
        res.check("a pixel-only session font resolves at the density the desktop has",
                  DU._rewrite_lxqt_font(lxqt, 96) == (12.0, 16),
                  open(lxqt).read())

        # Every LXQt application of this home watches its configuration from
        # whichever display it is on, so only the session on this display has
        # its font retargeted, in the configuration that session reads.
        in_points = '[Qt]\nfont="Sans,11,-1,5,50,0,0,0,0,0"\n'
        home_conf = os.path.join(home, ".config", "lxqt", "lxqt.conf")
        session_config = os.path.join(home, "session-config")
        session_conf = os.path.join(session_config, "lxqt", "lxqt.conf")
        for conf in (home_conf, session_conf):
            os.makedirs(os.path.dirname(conf), exist_ok=True)
            with open(conf, "w") as f:
                f.write(in_points)
        sessions = [H.named_process("lxqt-session", {"HOME": home, "DISPLAY": other})]
        try:
            res.check("a session of this home on another display keeps its font",
                      not asyncio.run(DU._run_lxqt_font(144, DU.logger_app_resize))
                      and open(home_conf).read() == in_points, open(home_conf).read())
            sessions.append(H.named_process("lxqt-session", {"HOME": home, "DISPLAY": display,
                                                             "XDG_CONFIG_HOME": session_config}))
            res.check("the session on this display has its font resolved where it reads it",
                      asyncio.run(DU._run_lxqt_font(144, DU.logger_app_resize))
                      and 'font="Sans,11,22,' in open(session_conf).read()
                      and open(home_conf).read() == in_points, open(session_conf).read())
        finally:
            for session in sessions:
                session.kill()
                session.wait()

        # A desktop's settings stores are written through the bus of the session
        # on this display, never through the one this process inherited, which
        # may be another display's session of this home. The tools are stand-ins
        # recording the bus each run was given.
        stub_bin = os.path.join(home, "stub-bin")
        written = os.path.join(home, "written")
        os.makedirs(stub_bin)
        for tool in ("xfconf-query", "gsettings"):
            with open(os.path.join(stub_bin, tool), "w") as f:
                f.write(f'#!/bin/sh\necho "{tool} $DBUS_SESSION_BUS_ADDRESS $*" >> "{written}"\n')
            os.chmod(os.path.join(stub_bin, tool), 0o755)
        saved_path, saved_bus = os.environ.get("PATH", ""), os.environ.get("DBUS_SESSION_BUS_ADDRESS")
        os.environ["PATH"] = stub_bin + os.pathsep + saved_path
        os.environ["DBUS_SESSION_BUS_ADDRESS"] = "unix:path=inherited"

        def runs() -> list:
            """The tool runs recorded since the last call, as `tool bus args`."""
            try:
                with open(written) as f:
                    lines = f.read().splitlines()
                os.unlink(written)
            except OSError:
                return []
            return lines

        def session(binary: str, on: str) -> subprocess.Popen:
            return H.named_process(binary, {"HOME": home, "DISPLAY": on,
                                            "DBUS_SESSION_BUS_ADDRESS": f"unix:path={on}"})

        desktops = []
        try:
            desktops += [session(b, other) for b in ("xfce4-session", "mate-session", "gsd-xsettings")]
            applied = (asyncio.run(DU._run_xfconf(144, DU.logger_app_resize)),
                       asyncio.run(DU._run_mate_gsettings(144, DU.logger_app_resize)),
                       asyncio.run(DU.set_cursor_size(48)))
            res.check("sessions of this home on another display have no store written through any bus",
                      applied == (False, False, True) and not runs(), (applied, runs()))
            desktops += [session(b, display) for b in ("mate-session", "gsd-xsettings")]
            cursor = asyncio.run(DU.set_cursor_size(48))
            res.check("a cursor size reaches the GNOME key through the bus of this display's session",
                      cursor and runs() == [f"gsettings unix:path={display} set org.gnome.desktop.interface "
                                            "cursor-size 48"], cursor)
            mate = asyncio.run(DU._run_mate_gsettings(144, DU.logger_app_resize))
            got = runs()
            res.check("a MATE density is written through the bus of this display's session",
                      mate and len(got) == 2 and all(r.startswith(f"gsettings unix:path={display} set org.mate.")
                                                     for r in got), got)
            desktops.append(session("xfce4-session", display))
            xfconf = asyncio.run(DU._run_xfconf(144, DU.logger_app_resize))
            got = runs()
            res.check("an XFCE density is written through the bus of this display's session",
                      xfconf and len(got) == 2 and all(r.startswith(f"xfconf-query unix:path={display} ")
                                                       for r in got), got)
            cursor = asyncio.run(DU.set_cursor_size(48))
            got = runs()
            res.check("an XFCE cursor size is written through the bus of this display's session",
                      cursor and got == [f"xfconf-query unix:path={display} -c xsettings -p "
                                         "/Gtk/CursorThemeSize -s 48 --create -t int"], got)
        finally:
            for desktop in desktops:
                desktop.kill()
                desktop.wait()
            os.environ["PATH"] = saved_path
            if saved_bus is None:
                os.environ.pop("DBUS_SESSION_BUS_ADDRESS", None)
            else:
                os.environ["DBUS_SESSION_BUS_ADDRESS"] = saved_bus

        # An XFCE session keeps its density in xfconf, the one store set_dpi
        # writes there, so a restart reads it before the database and the files.
        channel = os.path.join(home, ".config", "xfce4", "xfconf", "xfce-perchannel-xml")
        os.makedirs(channel)
        real_declared = DU._declared_desktop
        DU._declared_desktop = lambda: "XFCE"
        try:
            res.check("an XFCE session that persisted no density reads the desktop as any other",
                      DU.desktop_dpi() == 120, DU.desktop_dpi())
            for value, want, label in ((-1, 120, "xfconf's unset density falls through"),
                                       (192, 192, "the density xfconf persists is read over the database and the files")):
                with open(os.path.join(channel, "xsettings.xml"), "w") as f:
                    f.write('<channel name="xsettings" version="1.0">\n  <property name="Xft" type="empty">\n'
                            f'    <property name="DPI" type="int" value="{value}"/>\n  </property>\n</channel>\n')
                res.check(label, DU.desktop_dpi() == want, DU.desktop_dpi())
            DU._declared_desktop = lambda: "Openbox"
            res.check("another session never reads xfconf's channel", DU.desktop_dpi() == 120, DU.desktop_dpi())
        finally:
            DU._declared_desktop = real_declared

    real_set_dpi, real_desktop_dpi = DU.set_dpi, DU.desktop_dpi
    applied = []

    async def record(value):
        applied.append(int(value))
        return True

    DU.set_dpi = record
    try:
        for current, configured, locked, want in (
                (None, 96, False, []), (96, 96, False, []), (192, 96, False, [192]),
                (192, 96, True, [96]), (None, 96, True, []), (None, 192, True, [192]),
                (192, 192, True, [192]), (144, 192, True, [192])):
            DU.desktop_dpi = lambda c=current: c
            applied.clear()
            got = asyncio.run(DU.restore_dpi(configured, locked))
            settle = configured if locked else (current or configured)
            res.check(f"startup finds {current or 'no'} density, {configured} "
                      f"{'operator-set' if locked else 'configured'}: applies "
                      f"{want or 'nothing'}, settles at {settle}",
                      applied == want and got == settle, (applied, got))
    finally:
        DU.set_dpi, DU.desktop_dpi = real_set_dpi, real_desktop_dpi
finally:
    for k, v in saved_env.items():
        if v is None:
            os.environ.pop(k, None)
        else:
            os.environ[k] = v
    if xserver:
        H.stop_x_server(xserver, display)
    if other_server:
        H.stop_x_server(other_server, other)
    shutil.rmtree(home, ignore_errors=True)

sys.exit(0 if res.summary() else 1)
