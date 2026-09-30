#!/usr/bin/env python3
"""Text typed into a Wayland session reaches its browsers intact.

Unicode text and IME commits are typed as keys bound to keysyms a keymap swap
gives them: on pixelflux's own seat by the seat's keymap owner, and under a
nested app compositor (the images' labwc) by pixelflux's virtual-keyboard
client, whose keymap is its own. A browser reads such a key as a physical key
as well as for its keysym: Chromium drops a key whose code names no key it
knows and runs its browser, media, and launcher keys as commands, so a
character bound to a spare vendor keycode never arrived, or reloaded or
navigated the page instead. Each string is typed into a textarea of Chrome on
pixelflux's compositor, and of Chrome and Firefox on a headless labwc, and read
back; the longest has more distinct characters than either overlay has keys for
one swap, and the mixed one types ASCII on keys an earlier string bound.
Firefox reads a key for its keysym alone, and on pixelflux's output with no
capture running it takes no keys at all, so the seat half runs Chrome only.
"""
import os
import shutil
import subprocess
import sys
import tempfile
import time

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))
sys.path.insert(0, TESTS)
import core_lib as C  # noqa: E402
import helpers as H  # noqa: E402

from selkies import input_handler as IH  # noqa: E402

try:
    from pixelflux import ScreenCapture, ensure_wayland_display
except Exception as exc:
    H.skip_suite(f"pixelflux is not importable: {exc}")
from playwright.sync_api import Error as PlaywrightError, sync_playwright  # noqa: E402

CASES = [
    ("the reported commit", "大模型"),
    ("the same commit again", "大模型"),
    ("Chinese", "测试一下中文输入"),
    ("Japanese", "日本語のテキスト"),
    ("Korean", "한국어 입력"),
    ("more characters than a swap binds", "".join(chr(0x4E00 + 13 * i) for i in range(64))),
    ("ASCII on keys an earlier string bound, among CJK", "大型123abc模 qwerty"),
    ("Latin-1 and beyond", "é ü ñ £ ¿ ÿ Ω ф"),
]
PAGE = '<textarea id="t" style="width:600px;height:300px" autofocus></textarea>'


def boot_labwc(runtime: str) -> tuple:
    """Start a headless labwc on a private runtime directory; the process and its socket."""
    env = dict(os.environ, XDG_RUNTIME_DIR=runtime, WLR_BACKENDS="headless",
               WLR_LIBINPUT_NO_DEVICES="1", WLR_RENDERER="pixman", LIBGL_ALWAYS_SOFTWARE="1")
    env.pop("WAYLAND_DISPLAY", None)
    env.pop("DISPLAY", None)
    # Xwayland inherits this: its glamor probe segfaults inside the NVIDIA EGL
    # vendor when the renderer underneath is software.
    mesa = "/usr/share/glvnd/egl_vendor.d/50_mesa.json"
    if os.path.exists(mesa):
        env["__EGL_VENDOR_LIBRARY_FILENAMES"] = mesa
    log = open(os.path.join(runtime, "labwc.log"), "w")
    proc = H.spawn(["labwc"], env=env, stdout=log, stderr=subprocess.STDOUT)
    return proc, wayland_socket(runtime)


def wayland_socket(runtime: str, seconds: float = 30.0) -> str:
    """The first Wayland socket under `runtime`, waited for; empty if none came up."""
    deadline = time.time() + seconds
    while time.time() < deadline:
        socket = next((n for n in sorted(os.listdir(runtime))
                       if n.startswith("wayland-") and not n.endswith(".lock")), "")
        if socket:
            return socket
        time.sleep(0.25)
    return ""


def launch(pw, engine: str, env: dict):
    """A headed browser of `engine` on the compositor `env` names."""
    if engine == "firefox":
        return pw.firefox.launch(**C.installed_firefox(
            {"headless": False, "env": dict(env, MOZ_ENABLE_WAYLAND="1")}))
    kwargs = {"headless": False, "env": env,
              "args": C.BROWSER_ARGS + ["--ozone-platform=wayland", "--disable-gpu"]}
    if C.CHROME_PATH:
        kwargs["executable_path"] = C.CHROME_PATH
    return pw.chromium.launch(**kwargs)


def reset(page) -> None:
    """A fresh textarea page, focused. Focused rather than clicked: with no
    capture running, nothing repaints pixelflux's output, so a page there never
    settles for a click's stability wait."""
    page.goto("about:blank")
    page.set_content(PAGE)
    page.focus("#t")


def typed_back(page, type_text, text: str, seconds: float = 6.0) -> str:
    """Type `text` with `type_text` and return what the textarea holds once it
    matches or the time runs out; `<navigated>` when a key took the page away,
    after which a fresh one is put up."""
    page.evaluate("document.getElementById('t').value = ''")
    type_text(text)
    got, deadline = "", time.time() + seconds
    try:
        while time.time() < deadline:
            got = page.evaluate("document.getElementById('t').value")
            if got == text:
                break
            time.sleep(0.1)
        if page.url == "about:blank":
            return got
    except PlaywrightError:
        time.sleep(1.0)
    reset(page)
    return "<navigated>"


def run(res: "H.Results", pw, where: str, socket: str, runtime: str, type_text,
        engines: tuple = ("chromium", "firefox")) -> None:
    """Every case into each of `engines` on the compositor at `socket`."""
    env = dict(os.environ, WAYLAND_DISPLAY=socket, XDG_RUNTIME_DIR=runtime)
    env.pop("DISPLAY", None)
    for engine in engines:
        browser = launch(pw, engine, env)
        try:
            page = browser.new_page()
            reset(page)
            time.sleep(1.0)
            for label, text in CASES:
                got = typed_back(page, type_text, text)
                res.check(f"{where}, {engine}: {label} arrives whole", got == text, repr(got[:80]))
        finally:
            browser.close()


def main() -> int:
    res = H.Results("wayland-typed-text")
    with sync_playwright() as pw:
        ensure_wayland_display(width=1280, height=720, render_node="", auto_gpu="", cursor_size=24)
        capture = ScreenCapture()
        seat_runtime = os.environ["XDG_RUNTIME_DIR"]
        owner = IH._WaylandKeymapOwner(capture, capture.get_xkb_keymap_string())
        run(res, pw, "seat", wayland_socket(seat_runtime), seat_runtime, owner.type_text, ("chromium",))

        if shutil.which("labwc") is None:
            print("SKIP  [wayland-typed-text] the nested checks need labwc", flush=True)
            return 0 if res.summary() else 1
        runtime = tempfile.mkdtemp(prefix="wlkb-")
        os.chmod(runtime, 0o700)
        # The typer resolves the compositor socket under this process's own runtime
        # directory, so it has to be the one labwc is started in.
        os.environ["XDG_RUNTIME_DIR"] = runtime
        proc, socket = boot_labwc(runtime)
        try:
            res.check("the nested compositor comes up", bool(socket),
                      H.tail(os.path.join(runtime, "labwc.log")))
            if socket:
                # A seat gains its keyboard with the virtual one; a window mapped
                # before that would never take keyboard focus.
                capture.type_keysyms_wayland(socket, [])
                run(res, pw, "nested", socket, runtime,
                    lambda text: capture.type_keysyms_wayland(socket, IH.text_to_wayland_keysyms(text)))
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=10)
            except Exception:
                proc.kill()
            shutil.rmtree(runtime, ignore_errors=True)
    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
