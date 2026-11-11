"""11. Check that the session's desktop follows the page's light or dark color scheme.

The client browser's own preference is emulated (`prefers-color-scheme`) and
then overridden by the dashboard's theme toggle, as a user picking a theme
there does. What reaches the session is read where an application meets it: a
tester page in the session's own Chrome and Firefox reports the same media
query, live, and the settings portal is asked on the session's bus for the
`org.freedesktop.appearance color-scheme` it serves to GTK 4, libadwaita, and
Qt. Where the session has the bindings, a probe of each toolkit, started
before the first change and left running as an application would be
(`site/scheme_probe.py`), reports what GTK 3, GTK 4, libadwaita, and Qt read.
The session's browsers run with whatever environment the desktop hands its
applications, or none where an image runs no desktop, which is the pure-browser
case: there they reach the portal on `$XDG_RUNTIME_DIR/bus` alone.
"""
import time
from typing import Any, Optional

from image_lib import SITE_ROOT, open_sidebar, session_browser, stop_session_browser

ITEM = 11
TITLE = "desktop color scheme"
BROWSERS = ("chrome", "firefox")
PORTAL = ("gdbus call --session -d org.freedesktop.portal.Desktop -o /org/freedesktop/portal/desktop "
          "-m org.freedesktop.portal.Settings.ReadOne org.freedesktop.appearance color-scheme 2>&1")
# org.freedesktop.appearance's values: 0 no preference, 1 dark, 2 light.
PORTAL_VALUE = {True: "uint32 1", False: "uint32 2"}
# Each probe: its name, the toolkit, and the environment it starts in beside the
# session's. Qt reads a color scheme only through a platform theme that has one,
# and its generic theme has none, so Qt runs on its GNOME theme, which reads the
# portal (QT_QPA_PLATFORMTHEME=gnome, as a session selects it).
PROBES = (("gtk3", "gtk3", ""), ("gtk4", "gtk4", ""), ("adw", "adw", ""),
          ("qt", "qt", "QT_QPA_PLATFORMTHEME=gnome "))


def probe_dark(reading: dict) -> Optional[bool]:
    """Whether a toolkit probe's reading is dark, None where it names neither."""
    if "dark" in reading:
        return bool(reading["dark"])
    scheme = str(reading.get("scheme", "")).lower()
    if scheme in ("dark", "light"):
        return scheme == "dark"
    theme = str(reading.get("theme", ""))
    return theme.lower().endswith("-dark") if theme else None


def start_probes(cell: Any) -> list:
    """Start every toolkit probe the session has bindings for; the ones running. A first
    reading naming no scheme is a session that has none yet, not a toolkit that reads none."""
    running = []
    for name, kind, env in PROBES:
        cell.clear_report(f"probe-{name}")
        cell.target.sh(f"(setsid nohup env {env}python3 {SITE_ROOT}/site/scheme_probe.py {kind} "
                       f"{SITE_ROOT}/out/probe-{name}.json > {SITE_ROOT}/probe-{name}.log 2>&1 &)")
    for name, _, _ in PROBES:
        reading = cell.report(f"probe-{name}", 20)
        if reading is None or "missing" in reading:
            cell.res.skip(f"the {name} probe", (reading or {}).get("missing", "no reading"))
        else:
            running.append(name)
    return running


def stop_probes(cell: Any) -> None:
    cell.target.sh("pkill -f '[s]cheme_probe.py'; true")


def session_dark(cell: Any, which: str, want: bool, timeout: float = 30) -> Optional[bool]:
    """What the session's `which` reports for the dark query once it reads `want`, else its last report."""
    report = cell.report(f"scheme-{which}", timeout, where=lambda r: r.get("dark") == want)
    if report is None:
        report = cell.report(f"scheme-{which}", 1)
    return None if report is None else bool(report.get("dark"))


def portal_reads(cell: Any, want: bool, timeout: float = 20) -> str:
    """The portal's color-scheme reply once it is the one for `want`, else the last reply."""
    deadline, reply = time.time() + timeout, ""
    while time.time() < deadline:
        reply = cell.target.out(PORTAL, 40)
        if PORTAL_VALUE[want] in reply:
            break
        time.sleep(1)
    return reply


def expect(cell: Any, running: list, want: bool, why: str, probes: tuple = ()) -> None:
    """Every running session browser, toolkit probe, and the portal read `want`, `why` naming
    what asked for it."""
    R = cell.res
    scheme = "dark" if want else "light"
    for which in running:
        seen = session_dark(cell, which, want)
        R.check(f"{why}: the session's {which} prefers {scheme}", seen == want, f"it reports dark={seen}")
    for kind in probes:
        reading = cell.report(f"probe-{kind}", 20, where=lambda r: probe_dark(r) == want) \
            or cell.report(f"probe-{kind}", 1) or {}
        R.check(f"{why}: a running {kind} application reads {scheme}", probe_dark(reading) == want, str(reading))
    reply = portal_reads(cell, want)
    R.check(f"{why}: the settings portal serves {scheme}", PORTAL_VALUE[want] in reply, reply[-200:])


def toggle_theme(page: Any) -> Optional[str]:
    """Click the dashboard's theme toggle; the theme it stored."""
    open_sidebar(page)
    button = page.locator(".theme-toggle").first
    if not button.count():
        return None
    button.click()
    time.sleep(0.5)
    return page.evaluate("localStorage.getItem('theme')")


def run(cell: Any) -> None:
    R = cell.res
    running = []
    for which in BROWSERS:
        cell.clear_report(f"scheme-{which}")
        if session_browser(cell.target, which, "/scheme.html", kiosk=False, profile=which):
            running.append(which)
        else:
            R.skip(f"the session's {which}", "not installed in this image")
    probes: tuple = ()
    try:
        probes = tuple(start_probes(cell))
        page = cell.open()
        for which in running:
            R.check(f"the session's {which} runs the scheme tester",
                    cell.report(f"scheme-{which}", 45) is not None, "no report from it")
        for want in (True, False, True):
            page.emulate_media(color_scheme="dark" if want else "light")
            expect(cell, running, want, f"a browser preferring {'dark' if want else 'light'}", probes)

        # The dashboard's theme, once picked, governs over the browser's preference.
        page.emulate_media(color_scheme="dark")
        stored = toggle_theme(page)
        R.check("the dashboard's toggle stores a pick", stored in ("light", "dark"), f"stored {stored!r}")
        if stored in ("light", "dark"):
            expect(cell, running, stored == "dark", f"a dashboard theme picked {stored} under a dark browser")
            page.emulate_media(color_scheme="light" if stored == "dark" else "dark")
            time.sleep(3)
            expect(cell, running, stored == "dark", "the pick holding as the browser's preference flips")
            stored = toggle_theme(page)
            expect(cell, running, stored == "dark", f"the dashboard's theme picked {stored} again")
        log = cell.target.selkies_log(4000)
        R.check("the server says what the desktop's scheme became",
                "Desktop color scheme set to" in log,
                ([ln for ln in log.splitlines() if "color scheme" in ln] or ["no line"])[-1][-200:])
    finally:
        stop_probes(cell)
        for which in running:
            stop_session_browser(cell.target, which, which)
