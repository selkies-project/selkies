#!/usr/bin/env python3
"""What the container does with the backend its GPU probe names.

The probe brings the compositor's own renderer up, so how it exits says as much
as what it prints: a report that could not be had exits 1 and leaves the session
on the backend it was asked for, while a bring-up that wedges or dies on a
signal is the one the session is about to attempt, and starting it regardless is
a supervisor restarting a crash with no session at all to show for it. The
deciding block runs here verbatim out of the entrypoint, against a stand-in
probe, under the same `set -e` the entrypoint runs with.
"""
import os
import subprocess
import sys
import tempfile

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
ENTRYPOINT = os.path.join(REPO, "addons", "base", "container-entrypoint.sh")
sys.path.insert(0, TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))
import helpers as H  # noqa: E402

from selkies.gpu_probe import session_environment  # noqa: E402

BLOCK_START = 'if [ "${SELKIES_WAYLAND}" = "true" ]; then\n  probe_status=0'


def entrypoint_block() -> str:
    """The probe-verdict block and the parsing helpers it calls, verbatim."""
    body = open(ENTRYPOINT, encoding="utf-8").read()
    parts = []
    for name in ("setting_value() {", "is_true() {"):
        begin = body.index(name)
        parts.append(body[begin:body.index("\n}\n", begin) + 3])
    begin = body.index(BLOCK_START)
    parts.append(body[begin:body.index("\nfi\n", begin) + 4])
    return "\n".join(parts)


def verdict(stub: str, env: dict) -> dict:
    """Run the block with `stub` standing in for the probe; report what it settled.

    Args:
        stub: Shell body of the stand-in `selkies-gpu-probe`.
        env: Environment the block starts from.

    Returns:
        The resulting `wayland`, `gl` (the Zink override) and `said` (stdout).
    """
    with tempfile.TemporaryDirectory() as tmp:
        probe = os.path.join(tmp, "selkies-gpu-probe")
        with open(probe, "w") as fh:
            fh.write("#!/bin/bash\n" + stub + "\n")
        os.chmod(probe, 0o755)
        script = os.path.join(tmp, "block.sh")
        with open(script, "w") as fh:
            fh.write("set -e\n" + entrypoint_block()
                     + '\necho "WAYLAND=${SELKIES_WAYLAND} GL=${GALLIUM_DRIVER-unset}"\n')
        run = subprocess.run(
            ["bash", script], capture_output=True, text=True, timeout=120,
            env=dict(os.environ, PATH=tmp + os.pathsep + os.environ["PATH"],
                     GALLIUM_DRIVER="zink", MESA_LOADER_DRIVER_OVERRIDE="zink",
                     LIBGL_KOPPER_DRI2="1", **env))
        state = run.stdout.strip().splitlines()[-1] if run.stdout.strip() else ""
        return {"wayland": "WAYLAND=true" in state, "gl": "GL=zink" in state,
                "said": run.stdout, "rc": run.returncode}


res = H.Results("backend-verdict")
ON = {"SELKIES_WAYLAND": "true"}

named = verdict('echo x11', ON)
res.check("a probe naming x11 moves the session off Wayland",
          not named["wayland"] and named["rc"] == 0, named["said"].strip()[:80])

kept = verdict('echo wayland', ON)
res.check("a probe naming wayland leaves the session and its GL stack alone",
          kept["wayland"] and kept["gl"], kept["said"].strip()[:80])

soft = verdict('echo wayland-software', ON)
res.check("a software verdict drops the Zink override the compositor cannot feed",
          soft["wayland"] and not soft["gl"], soft["said"].strip()[:80])

silent = verdict('exit 1', ON)
res.check("a report that could not be had leaves the ask standing",
          silent["wayland"] and silent["gl"] and silent["rc"] == 0,
          silent["said"].strip()[:80])

crashed = verdict('kill -SEGV $$', ON)
res.check("a bring-up that dies on a signal starts X11 instead",
          not crashed["wayland"] and crashed["rc"] == 0, crashed["said"].strip()[:120])
res.check("and says so", "did not survive" in crashed["said"], crashed["said"].strip()[:120])

# timeout(1) answers 124 for a command it had to stop; the stand-in exits with
# that status rather than holding the suite for the full minute.
wedged = verdict('exit 124', ON)
res.check("a bring-up that wedges starts X11 too",
          not wedged["wayland"], wedged["said"].strip()[:120])

no_x11 = verdict('kill -SEGV $$', dict(ON, SELKIES_WAYLAND_X11_FALLBACK="false"))
res.check("a session that declined X11 composites in software instead",
          no_x11["wayland"] and not no_x11["gl"], no_x11["said"].strip()[:120])

off = verdict('echo x11', {"SELKIES_WAYLAND": "false"})
res.check("an X11 session never asks", off["said"].strip().endswith("GL=zink")
          and "did not survive" not in off["said"], off["said"].strip()[:80])

# The probe reads this file when it is run by hand, so what it parses out of it
# has to be what the entrypoint wrote: printf %q quoting, one export per line.
with tempfile.TemporaryDirectory() as tmp:
    recorded = os.path.join(tmp, "container-env")
    subprocess.run(["bash", "-c",
                    'FOO="a b" BAR= BAZ=plain; export FOO BAR BAZ; '
                    'env | grep -E "^(FOO|BAR|BAZ)=" | sort | '
                    'while IFS= read -r kv; do printf "export %s=%q\\n" "${kv%%=*}" "${kv#*=}"; done'],
                   stdout=open(recorded, "w"), timeout=60, check=True)
    parsed = session_environment(recorded)
    res.check("the recorded environment parses back to what was exported",
              parsed.get("FOO") == "a b" and parsed.get("BAR") == "" and parsed.get("BAZ") == "plain",
              parsed)
res.check("a missing recorded environment is no environment",
          session_environment(os.path.join(tmp, "gone")) == {})

sys.exit(0 if res.summary() else 1)
