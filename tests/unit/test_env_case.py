#!/usr/bin/env python3
"""Environment values are read the same way wherever they are read.

An operator types SELKIES_WAYLAND=True once and expects it to mean the same thing
to the container entrypoint, to the tools it calls, and to the server it starts.
Where those disagree the deployment is configured two ways at once and nothing
reports it: the entrypoint starts an X11 session while the server captures
Wayland, or a transport name in the wrong case takes down the server on a key
lookup. So the shell and the Python have to agree on one rule -- "true" or "1",
case-insensitively, ahead of any "|locked" suffix -- and the values that name
something (a transport, an encoder) have to resolve however they are cased.
"""
import os
import subprocess
import sys
import tempfile

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
ENTRYPOINT = os.path.join(REPO, "addons", "example", "container-entrypoint.sh")

sys.path.insert(0, os.path.join(REPO, "src"))
from selkies.settings import parse_bool  # noqa: E402

passed = failed = 0


def check(label, ok, detail=""):
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [env-case] {label}  {detail}", flush=True)


def shell_functions():
    """The entrypoint's own parsing helpers, extracted verbatim."""
    body = open(ENTRYPOINT).read()
    parts = []
    for name in ("setting_value() {", "is_true() {"):
        begin = body.index(name)
        parts.append(body[begin:body.index("\n}\n", begin) + 3])
    return "\n".join(parts)


def run_shell(script, env=None, *args):
    with tempfile.TemporaryDirectory() as tmp:
        path = os.path.join(tmp, "lib.sh")
        with open(path, "w") as f:
            f.write(shell_functions() + "\n" + script + "\n")
        return subprocess.run(["bash", path, *args], capture_output=True,
                              text=True, timeout=60, env=env).stdout.strip()


def shell_is_true(value):
    return run_shell('if is_true "$1"; then echo yes; else echo no; fi',
                     None, value) == "yes"


# Every spelling an operator might reasonably type, and the ones that must stay
# false — including internal whitespace, which trimming must not erase.
TRUE_FORMS = ["true", "True", "TRUE", "tRuE", "1", " true ", "true|locked",
              "TRUE|LOCKED", " 1 |x"]
FALSE_FORMS = ["false", "False", "FALSE", "0", "no", "yes", "on", "", "  ",
               "truthy", "t rue", "true stuff", "|true"]

for value in TRUE_FORMS:
    check(f"python reads {value!r} as true", parse_bool(value) is True)
    check(f"shell reads {value!r} as true", shell_is_true(value))
for value in FALSE_FORMS:
    check(f"python reads {value!r} as false", parse_bool(value) is False)
    check(f"shell reads {value!r} as false", not shell_is_true(value))

# The backend switch resolves SELKIES_WAYLAND ahead of the legacy spelling the
# way settings.py does: a variable that is set but blank is still an override,
# neutralizing to the default rather than falling through to the other name.
CHAIN_LINE = next(line for line in open(ENTRYPOINT)
                  if 'is_true "${SELKIES_WAYLAND' in line)
CHAIN_EXPR = CHAIN_LINE.strip().removeprefix("if ").removesuffix("; then")
CHAIN_CASES = [
    (None, None, "x11"),
    (None, "True|locked", "wayland"),
    ("", "true", "x11"),
    ("  ", "TRUE", "x11"),
    ("tRuE", None, "wayland"),
    ("false", "true", "x11"),
]
for sel, legacy, expected in CHAIN_CASES:
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin")}
    if sel is not None:
        env["SELKIES_WAYLAND"] = sel
    if legacy is not None:
        env["PIXELFLUX_WAYLAND"] = legacy
    out = run_shell(f'if {CHAIN_EXPR}; then echo wayland; else echo x11; fi', env)
    check(f"chain SELKIES_WAYLAND={sel!r} PIXELFLUX_WAYLAND={legacy!r} -> {expected}",
          out == expected, out)
    raw = sel if sel is not None else (legacy if legacy is not None else "false")
    py = "wayland" if parse_bool("false" if raw.strip() == "" else raw) else "x11"
    check(f"python resolves the same chain to {expected}", py == expected, py)

# The session compositor follows the same reading: trimmed, case-insensitive,
# |suffix ignored, so a value reads the same however it was typed.
body = open(ENTRYPOINT).read()
begin = body.index('SELKIES_WAYLAND_COMPOSITOR="$(setting_value')
COMPOSITOR_BLOCK = body[begin:body.index(
    "\n", body.index('SELKIES_WAYLAND_COMPOSITOR="${SELKIES_WAYLAND_COMPOSITOR,,}"', begin))]
for value, expected in [
    (" LabWC |locked", "labwc"),
    ("NONE", "none"),
    ("  sway  ", "sway"),
    ("", ""),
]:
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"),
           "SELKIES_WAYLAND_COMPOSITOR": value}
    out = run_shell(COMPOSITOR_BLOCK + '\necho "${SELKIES_WAYLAND_COMPOSITOR}"', env)
    check(f"compositor {value!r} -> {expected!r}", out == expected, out)

# An empty value keeps the default, which is what lets an image neutralize a
# variable baked into a base layer without knowing what the default is.
check("an empty value keeps the default", parse_bool("", default=True) is True
      and parse_bool(None, default=True) is True)
check("a set value overrides the default", parse_bool("false", default=True) is False)

# The transport names a service registry key, so a mis-cased value used to abort
# the server on lookup rather than selecting the transport it names.
for value in ("webrtc", "WebRTC", "WEBRTC", " WebRTC "):
    out = subprocess.run(
        [sys.executable, "-c",
         "import selkies.settings as s; print(s.settings.mode)"],
        capture_output=True, text=True, timeout=120,
        env=dict(os.environ, PYTHONPATH=os.path.join(REPO, "src"),
                 SELKIES_MODE=value)).stdout.strip()
    check(f"mode {value!r} resolves to webrtc", out == "webrtc", out)

out = subprocess.run(
    [sys.executable, "-c", "import selkies.settings as s; print(s.settings.mode)"],
    capture_output=True, text=True, timeout=120,
    env=dict(os.environ, PYTHONPATH=os.path.join(REPO, "src"),
             SELKIES_MODE="carrier-pigeon")).stdout.strip()
check("an unknown mode falls back rather than aborting later", out == "websockets", out)

# An encoder named in the wrong case must select that encoder, not be dropped as
# unknown and silently replaced by the default.
out = subprocess.run(
    [sys.executable, "-c", "import selkies.settings as s; print(s.settings.encoder)"],
    capture_output=True, text=True, timeout=120,
    env=dict(os.environ, PYTHONPATH=os.path.join(REPO, "src"),
             SELKIES_ENCODER="JPEG")).stdout.strip()
check("an encoder resolves however it is cased", out == "jpeg", out)

# The historical alias has to keep working in either case, for the same reason.
out = subprocess.run(
    [sys.executable, "-c", "import selkies.settings as s; print(s.settings.encoder)"],
    capture_output=True, text=True, timeout=120,
    env=dict(os.environ, PYTHONPATH=os.path.join(REPO, "src"),
             SELKIES_ENCODER="X264enc")).stdout.strip()
check("the x264enc alias resolves however it is cased", out == "h264enc", out)

print(f"[env-case] {passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
