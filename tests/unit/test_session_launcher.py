#!/usr/bin/env python3
"""selkies-session finds the host's desktops and reads the backend as Selkies does.

The launcher runs on hosts it knows nothing about, so it adapts to them: the
desktop comes from the session files a display manager would list, for the
backend in effect, chosen by file, desktop, or program name, by
XDG_CURRENT_DESKTOP, by the default the host's display manager or session
alternative records, or else the session named after its own desktop; a name
only the other backend's sessions carry stands for their desktop, and one no
file carries runs as a command. The backend is Selkies' own setting, read by Selkies' parser,
so SELKIES_WAYLAND, its legacy name, and --wayland mean what they mean to the
server. The X authority file holds one wildcard cookie, which the vendored
Xlib must match on any display as libXau does. The package registers the
console script and the Jupyter server-proxy entry.
"""
import os
import re
import struct
import sys
import tempfile

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))

from selkies import session  # noqa: E402
from selkies.Xlib import xauth  # noqa: E402

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    ok = bool(ok)
    passed, failed = passed + int(ok), failed + int(not ok)
    print(f"{'PASS' if ok else 'FAIL'}  [session-launcher] {label}  {detail}", flush=True)


def write(path: str, text: str) -> None:
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as fh:
        fh.write(text)


def entry(name: str, exec_: str, extra: str = "") -> str:
    return f"[Desktop Entry]\nName={name}\nExec={exec_}\n{extra}\n[Desktop Action other]\nExec=wrong\n"


def with_env(**values):
    """Set or (for None) unset variables, returning what to restore."""
    saved = {k: os.environ.get(k) for k in values}
    for k, v in values.items():
        if v is None:
            os.environ.pop(k, None)
        else:
            os.environ[k] = v
    return saved


def main() -> bool:
    root = tempfile.mkdtemp(prefix="sessions-")
    home, system = os.path.join(root, "home"), os.path.join(root, "system")
    write(f"{system}/xsessions/plasmax11.desktop", entry("Plasma (X11)", "startplasma-x11", "DesktopNames=KDE"))
    write(f"{system}/xsessions/xfce.desktop", entry("Xfce", "startxfce4 --flag", "DesktopNames=XFCE;"))
    write(f"{system}/xsessions/gone.desktop", entry("Gone", "gone", "TryExec=/nonexistent/gone"))
    write(f"{system}/xsessions/hidden.desktop", entry("Hidden", "hidden", "Hidden=true"))
    write(f"{system}/xsessions/notes.txt", "not a session")
    write(f"{system}/wayland-sessions/plasma.desktop", entry("Plasma", "startplasma-wayland", "DesktopNames=KDE"))
    write(f"{home}/xsessions/xfce.desktop", entry("Xfce mine", "my-xfce", "DesktopNames=XFCE"))
    saved = with_env(XDG_DATA_HOME=home, XDG_DATA_DIRS=system, XDG_CURRENT_DESKTOP=None)
    # The host's own defaults stay out: each check names the ones it means
    host: list = []
    configured, session.configured_desktops = session.configured_desktops, lambda: host
    try:
        x11 = session.installed_sessions(False)
        check("X11 sessions are the xsessions files, hidden ones and missing TryExec left out",
              sorted(x11) == ["plasmax11", "xfce"], sorted(x11))
        check("the user's data directory shadows a system session of the same name",
              x11.get("xfce", {}).get("Exec") == "my-xfce", x11.get("xfce"))
        check("keys come from the [Desktop Entry] group alone",
              x11.get("plasmax11", {}).get("Exec") == "startplasma-x11")
        check("Wayland sessions are the wayland-sessions files", sorted(session.installed_sessions(True)) == ["plasma"])

        check("a session by file name", session.pick_session("plasmax11", False)[0] == "plasmax11")
        check("a session by desktop name, any case, follows the backend",
              session.pick_session("kde", False)[0] == "plasmax11" and session.pick_session("KDE", True)[0] == "plasma")
        check("a session by the program it runs", session.pick_session("/usr/bin/my-xfce", False)[0] == "xfce")
        check("a name only the other backend's sessions carry stands for their desktop",
              session.pick_session("plasma", False)[0] == "plasmax11"
              and session.pick_session("startplasma-x11", True)[0] == "plasma")
        check("a name no session file carries runs as a command",
              session.pick_session("firefox --kiosk", False) == ("", {"Exec": "firefox --kiosk"}))
        check("a command line runs as given, even one a session file runs",
              session.pick_session("my-xfce --x", False) == ("", {"Exec": "my-xfce --x"}))
        check("unset, a session named after its own desktop comes before the first by name",
              session.pick_session(None, False)[0] == "xfce")
        host[:] = ["", "plasma", "", "/usr/bin/startplasma-x11"]
        check("unset, the host's default, for either backend",
              session.pick_session(None, False)[0] == "plasmax11" and session.pick_session(None, True)[0] == "plasma")
        with_env(XDG_CURRENT_DESKTOP="ubuntu:XFCE")
        check("XDG_CURRENT_DESKTOP names the desktop ahead of the host's default",
              session.pick_session(None, False)[0] == "xfce")
        check("--session outranks both", session.pick_session("KDE", False)[0] == "plasmax11")
        host[:] = []
        with_env(XDG_CURRENT_DESKTOP="GNOME")
        check("a desktop XDG_CURRENT_DESKTOP names but none installed falls through",
              session.pick_session(None, False)[0] == "xfce")
        with_env(XDG_DATA_HOME=os.path.join(root, "empty"), XDG_DATA_DIRS=os.path.join(root, "empty"))
        check("nothing installed, nothing wanted: no session", session.pick_session(None, True) == ("", {}))
    finally:
        with_env(**saved)
        session.configured_desktops = configured

    etc = os.path.join(root, "host")
    write(f"{etc}/usr/share/lightdm/lightdm.conf.d/50-packaged.conf", "[Seat:*]\nuser-session=packaged\n")
    write(f"{etc}/etc/lightdm/lightdm.conf.d/10-site.conf", "[Seat:*]\nuser-session=site\n")
    write(f"{etc}/etc/lightdm/lightdm.conf", "[Seat:*]\n#user-session=commented\n")
    write(f"{etc}/etc/gdm/custom.conf", "[daemon]\nFallbackSession=gnome-xorg\n")
    write(f"{etc}/usr/bin/startxfce4", "")
    write(f"{etc}/etc/sysconfig/desktop", 'DESKTOP="KDE"\nPREFERRED=/usr/bin/startkde\n')
    os.makedirs(f"{etc}/etc/alternatives")
    os.symlink(f"{etc}/usr/bin/startxfce4", f"{etc}/etc/alternatives/x-session-manager")
    write(f"{home}/.dmrc", "[Desktop]\nSession=xfce\n")
    saved = with_env(HOME=home)
    try:
        found = session.configured_desktops(etc)
    finally:
        with_env(**saved)
    check("the host's defaults: the user's last choice, LightDM's with /etc outranking the packaged file, "
          "GDM's fallback, Debian's session manager, and Red Hat's desktop",
          found == ["xfce", "site", "gnome-xorg", f"{etc}/usr/bin/startxfce4", "KDE", "/usr/bin/startkde"], found)
    check("a host that records none gives none", not any(session.configured_desktops(os.path.join(root, "empty"))))

    opts, rest = session.parse(["--session", "xfce", "--port=1", "--", "--subfolder=/x"])
    check("--session is the launcher's and the rest goes to selkies",
          opts.session == "xfce" and rest == ["--port=1", "--", "--subfolder=/x"], rest)
    check("a leading -- is dropped", session.parse(["--", "--wayland"])[1] == ["--wayland"])
    check("no abbreviation takes a selkies flag", session.parse(["--sess"])[0].session is None)

    saved = with_env(SELKIES_WAYLAND=None, PIXELFLUX_WAYLAND=None)
    try:
        check("the backend defaults to X11", not session.selkies_settings([]).wayland[0])
        with_env(SELKIES_WAYLAND="true")
        check("SELKIES_WAYLAND picks Wayland", session.selkies_settings([]).wayland[0])
        check("--wayland=false on the command line outranks it", not session.selkies_settings(["--wayland=false"]).wayland[0])
        with_env(SELKIES_WAYLAND=None, PIXELFLUX_WAYLAND="1")
        check("the legacy PIXELFLUX_WAYLAND is read as Selkies reads it", session.selkies_settings([]).wayland[0])
        with_env(PIXELFLUX_WAYLAND=None)
        check("a bare --wayland picks Wayland", session.selkies_settings(["--wayland"]).wayland[0])
        check("the launcher's own argv is restored", "--wayland" not in sys.argv)
    finally:
        with_env(**saved)

    cookie = os.urandom(16)
    path = os.path.join(root, "Xauthority")
    with open(path, "wb") as fh:
        fh.write(struct.pack(">HHHH18sH16s", 0xFFFF, 0, 0, 18, b"MIT-MAGIC-COOKIE-1", 16, cookie))
    auth = xauth.Xauthority(path)
    got = [auth.get_best_auth(xauth.FamilyLocal, b"somehost", n) for n in (0, 64, 1023)]
    check("the vendored Xlib takes a wildcard cookie for any host and display",
          all(g == (b"MIT-MAGIC-COOKIE-1", cookie) for g in got))
    check("the cookie entry is the one the launcher writes", open(path, "rb").read()[:2] == b"\xff\xff")
    check("the launcher's Xvfb flags stay within what every Xvfb takes",
          set(a for a in session.XVFB_ARGS if a.startswith(("-", "+")))
          <= {"-screen", "-nolisten", "-noreset", "-s", "-dpms", "+extension"}, session.XVFB_ARGS)

    saved = with_env(SELKIES_PRELOAD="/usr/lib/libxcb.so.1")
    try:
        preloaded = session.for_selkies({"LD_PRELOAD": "/lib/other.so", "A": "1"})
        with_env(SELKIES_PRELOAD=None)
        plain = session.for_selkies({"A": "1"})
    finally:
        with_env(**saved)
    check("SELKIES_PRELOAD reaches Selkies alone, ahead of what it already preloads",
          preloaded.get("LD_PRELOAD") == "/usr/lib/libxcb.so.1:/lib/other.so" and plain == {"A": "1"}, preloaded)

    saved = with_env(XDG_RUNTIME_DIR="/nonexistent/runtime")
    try:
        missing = session.Session()
    finally:
        with_env(**saved)
    check("a runtime directory the environment names but nobody made falls back to the temp directory",
          os.path.dirname(missing.runtime_dir) == tempfile.gettempdir() and os.path.isdir(missing.runtime_dir)
          and missing.env["XDG_RUNTIME_DIR"] == missing.runtime_dir, missing.runtime_dir)
    os.rmdir(missing.runtime_dir)

    entry_ = session.jupyter()
    check("the Jupyter entry runs the launcher on the proxy's Unix socket, following the server, without a login",
          entry_["command"][3:] == ["--unix-socket", "{unix_socket}", "--enable-basic-auth=false", "--enable-https=false"]
          and "main(follow=True)" in entry_["command"][2] and entry_["unix_socket"] is True, entry_["command"])
    icon = entry_["launcher_entry"]["icon_path"]
    check("the desktop opens in a tab of its own, with the icon the dashboard build puts in the package",
          entry_["new_browser_tab"] is True and icon.endswith(os.path.join("selkies", "selkies_web", "selkies.svg"))
          and os.path.isfile(os.path.join(REPO, "addons", "selkies-dashboard", "public", "selkies.svg")), icon)

    with tempfile.TemporaryDirectory() as tmp:
        sockets = os.path.join(tmp, ".X11-unix")
        session.x11_socket_dir(sockets)
        mode = os.stat(sockets).st_mode & 0o7777
        check("the X11 socket directory a nested XWayland needs is made world-writable and sticky",
              mode == 0o1777, oct(mode))

    text = open(os.path.join(REPO, "pyproject.toml"), encoding="utf-8").read()
    check("the console script is registered", re.search(r'^selkies-session = "selkies\.session:main"$', text, re.M))
    check("the Jupyter server-proxy group names the entry",
          re.search(r'^\[project\.entry-points\.jupyter_serverproxy_servers\]\nselkies = "selkies\.session:jupyter"$',
                    text, re.M))
    check("the jupyter extra brings the proxy", re.search(r'^jupyter = \[\n\s*"jupyter-server-proxy[^"]*",?\n\]', text, re.M))

    print(f"[session-launcher] {passed}/{passed + failed} passed", flush=True)
    return failed == 0


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
