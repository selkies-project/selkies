#!/usr/bin/env python3
"""Text typed into a nested compositor reaches its XWayland clients intact.

The virtual-keyboard typer binds one keysym per character to a spare keycode,
so which keysym spells a character decides whether an X client can turn the key
back into text. Latin-1 has to keep its own keysym: spelling it in the Unicode
plane instead (0x010000E9 for e-acute) hands a client that looks keys up
through an input context the bare Latin-1 byte, which a UTF-8 client drops --
losing exactly the accented characters while everything else arrives. Native
Wayland clients decode both spellings, so only XWayland shows the difference.

Runs a headless labwc with its own XWayland and reads the text back out of xev,
whose XmbLookupString line is the lookup a toolkit does.
"""
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))
sys.path.insert(0, TESTS)
import helpers as H  # noqa: E402

from selkies import input_handler as IH  # noqa: E402

SAMPLE = "aé ü ñ £ ¿ ÿ Ω ф 中 \U0001F600 Z9"
BYTES = re.compile(r"XmbLookupString gives \d+ bytes: \(([0-9a-f ]*)\)")

if shutil.which("labwc") is None or shutil.which("xev") is None:
    H.skip_suite("a nested compositor test needs labwc and xev")
try:
    from pixelflux import ScreenCapture
except Exception as exc:
    H.skip_suite(f"pixelflux is not importable: {exc}")


def boot(runtime: str) -> tuple:
    """Start a headless labwc with XWayland, and report it and its displays.

    Returns:
        The process, its Wayland socket name and the X display it serves.
    """
    startup = os.path.join(runtime, "startup.sh")
    dump = os.path.join(runtime, "env")
    with open(startup, "w") as fh:
        fh.write(f"#!/bin/bash\nenv | grep ^DISPLAY= > {dump}\nexec sleep 3600\n")
    os.chmod(startup, 0o755)
    env = dict(os.environ, XDG_RUNTIME_DIR=runtime, WLR_BACKENDS="headless",
               WLR_LIBINPUT_NO_DEVICES="1", WLR_RENDERER="pixman",
               LIBGL_ALWAYS_SOFTWARE="1")
    # Xwayland inherits this: its glamor probe segfaults inside the NVIDIA EGL
    # vendor when the renderer underneath is software.
    mesa = "/usr/share/glvnd/egl_vendor.d/50_mesa.json"
    if os.path.exists(mesa):
        env["__EGL_VENDOR_LIBRARY_FILENAMES"] = mesa
    log = open(os.path.join(runtime, "labwc.log"), "w")
    proc = H.spawn(["labwc", "-s", startup], env=env, stdout=log, stderr=subprocess.STDOUT)
    display, socket = "", ""
    for _ in range(120):
        if os.path.exists(dump) and not socket:
            socket = next((n for n in sorted(os.listdir(runtime))
                           if n.startswith("wayland-") and not n.endswith(".lock")), "")
            display = open(dump).read().strip().split("=", 1)[-1]
        if socket and display:
            break
        time.sleep(0.25)
    return proc, socket, display


def typed_back(runtime: str, socket: str, display: str, keysyms: list) -> str:
    """Tap `keysyms` into the compositor and return what an XWayland client read."""
    env = dict(os.environ, XDG_RUNTIME_DIR=runtime, DISPLAY=display,
               LANG="C.UTF-8", LC_ALL="C.UTF-8")
    for _ in range(8):
        # Bytes, not text: xev echoes the raw bytes a lookup returned, and a
        # mis-spelled keysym is exactly the case where they are not UTF-8.
        xev = H.spawn(["xev", "-event", "keyboard"], env=env,
                      stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        time.sleep(1.5)
        if xev.poll() is None:
            break
        xev.wait()
    else:
        return "<no X client>"
    ScreenCapture().type_keysyms_wayland(socket, keysyms)
    time.sleep(1.0)
    xev.terminate()
    out = (xev.communicate(timeout=10)[0] or b"").decode("latin-1")
    raw = b"".join(bytes.fromhex(m.replace(" ", "")) for m in BYTES.findall(out))
    return raw.decode("utf-8", "replace")


res = H.Results("xwayland-typed-text")
RUNTIME = tempfile.mkdtemp(prefix="wlkb-")
os.chmod(RUNTIME, 0o700)
# The typer resolves the compositor socket under this process's own runtime
# directory, so it has to be the one labwc is started in.
os.environ["XDG_RUNTIME_DIR"] = RUNTIME
proc, SOCKET, DISPLAY = boot(RUNTIME)
try:
    if not SOCKET or not DISPLAY:
        H.skip_suite("the nested compositor did not come up with an XWayland display")
    keysyms = IH.text_to_wayland_keysyms(SAMPLE)
    got = typed_back(RUNTIME, SOCKET, DISPLAY, keysyms)
    res.check("typed text arrives whole in an XWayland client", got == SAMPLE, repr(got))

    # The spelling this policy rejects, measured rather than asserted: the same
    # characters in the Unicode plane come back as their raw Latin-1 bytes.
    plane = [0x01000000 | ord(c) for c in "éüñ"]
    got_plane = typed_back(RUNTIME, SOCKET, DISPLAY, plane)
    res.check("Latin-1 in the Unicode plane does not survive the lookup",
              got_plane != "éüñ", repr(got_plane))
finally:
    proc.terminate()
    try:
        proc.wait(timeout=10)
    except Exception:
        proc.kill()
    shutil.rmtree(RUNTIME, ignore_errors=True)

sys.exit(0 if res.summary() else 1)
