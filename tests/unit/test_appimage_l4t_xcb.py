#!/usr/bin/env python3
"""On L4T the AppImage hands selkies the system libxcb, and nothing else.

The NVIDIA X and EGL stack on a Jetson is linked against the distribution's
libxcb; resolving it to the copy the bundle carries instead kills the capture
thread with SIGSEGV seconds after capture starts, whatever the encoder. The
AppRun therefore preloads the system library -- but only where the platform
marker and a library are both there, and only into selkies, since the display
and sound servers it starts have no such conflict to fix and a preload exported
over them would reach every application the session runs.

The selection runs here as the AppImage runs it: the function is read out of
the AppRun the build writes and called with fabricated paths, so what passes is
the shipped text. Whether preloading cures the crash is a Jetson measurement
and is recorded in the issue, not here; there is no L4T host in this sandbox.
"""
import os
import subprocess
import sys
import tempfile

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, TESTS)

import helpers as H  # noqa: E402

REPO = os.path.dirname(TESTS)
APPIMAGE = os.path.join(REPO, "scripts", "ci", "appimage.sh")
res = H.Results("appimage-l4t-xcb")


def apprun() -> str:
    """The AppRun script the build writes, out of its heredoc."""
    text = open(APPIMAGE).read()
    body = text.split("cat > AppDir/AppRun <<'APPRUN'\n", 1)[1]
    return body.split("\nAPPRUN\n", 1)[0]


def selector(script: str) -> str:
    """Just the selection function, to call on its own."""
    start = script.index("first_present_if() {")
    return script[start:script.index("\n}\n", start) + 3]


def select(*args: str) -> str:
    """What the shipped function names for those paths, empty for none."""
    with tempfile.NamedTemporaryFile("w", suffix=".sh", delete=False) as f:
        f.write(selector(apprun()))
        path = f.name
    try:
        quoted = " ".join("'" + a + "'" for a in args)
        out = subprocess.run(["sh", "-c", f". {path}; first_present_if {quoted} || true"],
                             capture_output=True, text=True, timeout=20)
        return out.stdout
    finally:
        os.unlink(path)


tree = tempfile.mkdtemp(prefix="l4t-")
marker = os.path.join(tree, "nv_tegra_release")
first = os.path.join(tree, "aarch64", "libxcb.so.1")
second = os.path.join(tree, "lib", "libxcb.so.1")
for path in (first, second):
    os.makedirs(os.path.dirname(path), exist_ok=True)

open(first, "w").close()
res.check("a host that is not L4T is left alone", select(marker, first, second) == "",
          select(marker, first, second))

open(marker, "w").close()
res.check("on L4T the system library is named", select(marker, first, second) == first)

os.unlink(first)
open(second, "w").close()
res.check("the next path answers when the first is not there",
          select(marker, first, second) == second)

os.unlink(second)
res.check("an L4T host carrying none of them is left alone too",
          select(marker, first, second) == "", select(marker, first, second))

script = apprun()
res.check("the preload is never exported over the servers or the session",
          "export LD_PRELOAD" not in script)
res.check("the session launcher is handed it for Selkies alone",
          'export SELKIES_PRELOAD="${system_xcb}"' in script)
res.check("and a host without the marker still reaches the plain exec",
          script.rstrip().endswith('exec "${ENV_BIN}/selkies" "$@"'),
          script.rstrip()[-60:])
res.check("the marker the platform is read from is L4T's",
          "/etc/nv_tegra_release" in script)

os.rmdir(os.path.dirname(first))
os.rmdir(os.path.dirname(second))
os.unlink(marker)
os.rmdir(tree)

sys.exit(0 if res.summary() else 1)
