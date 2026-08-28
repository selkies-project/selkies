#!/usr/bin/env python3
"""What the container does with what its GPU probe reports.

Two answers come out of the one probe. The backend: it brings the compositor's
own renderer up, so how it exits says as much as what it prints — a report that
could not be had exits 1 and leaves the session on the backend it was asked for,
while a bring-up that wedges or dies on a signal is the one the session is about
to attempt, and starting it regardless is a supervisor restarting a crash with
no session at all to show for it. And the GPU: which one the session renders on
decides the GL stack its applications get, so a host with an NVIDIA card whose
session renders on an Intel node must not be handed Zink.

Both deciding blocks run here verbatim out of the entrypoint, against a stand-in
probe, under the same `set -e` the entrypoint runs with.
"""
import glob
import os
import shutil
import subprocess
import sys
import tempfile
from typing import Optional

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
ENTRYPOINT = os.path.join(REPO, "addons", "base", "container-entrypoint.sh")
sys.path.insert(0, TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))
import helpers as H  # noqa: E402

from selkies import gpu_probe as BV  # noqa: E402
from selkies.gpu_probe import session_environment  # noqa: E402

BLOCK_START = 'if [ "${SELKIES_WAYLAND}" = "true" ]; then\n  probe_status=0'
GL_BLOCK_START = 'session_gpu_env="$(timeout 60 selkies-gpu-probe --session-env)"'


def entrypoint_block(start: str = BLOCK_START) -> str:
    """A deciding block and the parsing helpers it calls, verbatim.

    Args:
        start: First line of the block to extract; it runs to its closing `fi`.
    """
    body = open(ENTRYPOINT, encoding="utf-8").read()
    parts = []
    for name in ("setting_value() {", "is_true() {"):
        begin = body.index(name)
        parts.append(body[begin:body.index("\n}\n", begin) + 3])
    begin = body.index(start)
    # Blocks are separated by a blank line and carry none inside them.
    parts.append(body[begin:body.index("\n\n", begin)])
    return "\n".join(parts)


def run_block(stub: str, env: dict, block: str, report: str) -> dict:
    """Run one entrypoint block with `stub` standing in for the probe.

    Args:
        stub: Shell body of the stand-in `selkies-gpu-probe`.
        env: Environment the block starts from.
        block: First line of the block to extract.
        report: Shell echoing the state the checks read back.

    Returns:
        `stdout` (everything the block said) and `state` (the report line).
    """
    with tempfile.TemporaryDirectory() as tmp:
        probe = os.path.join(tmp, "selkies-gpu-probe")
        with open(probe, "w") as fh:
            fh.write("#!/bin/bash\n" + stub + "\n")
        os.chmod(probe, 0o755)
        script = os.path.join(tmp, "block.sh")
        with open(script, "w") as fh:
            fh.write("set -e\n" + entrypoint_block(block) + "\n" + report + "\n")
        run = subprocess.run(
            ["bash", script], capture_output=True, text=True, timeout=120,
            env=dict(os.environ, PATH=tmp + os.pathsep + os.environ["PATH"], **env))
        lines = [ln for ln in run.stdout.strip().splitlines() if ln]
        return {"stdout": run.stdout, "state": lines[-1] if lines else "", "rc": run.returncode}


def verdict(stub: str, env: dict) -> dict:
    """Run the block with `stub` standing in for the probe; report what it settled.

    Args:
        stub: Shell body of the stand-in `selkies-gpu-probe`.
        env: Environment the block starts from.

    Returns:
        The resulting `wayland`, `gl` (the Zink override) and `said` (stdout).
    """
    out = run_block(stub, dict(env, GALLIUM_DRIVER="zink",
                               MESA_LOADER_DRIVER_OVERRIDE="zink", LIBGL_KOPPER_DRI2="1"),
                    BLOCK_START, 'echo "WAYLAND=${SELKIES_WAYLAND} GL=${GALLIUM_DRIVER-unset}"')
    return {"wayland": "WAYLAND=true" in out["state"], "gl": "GL=zink" in out["state"],
            "said": out["stdout"], "rc": out["rc"]}


def gl_verdict(stub: str, env: Optional[dict] = None) -> dict:
    """Run the GL block with `stub` reporting for the probe; report what it set."""
    out = run_block(stub, dict(env or {}, GALLIUM_DRIVER="", MESA_LOADER_DRIVER_OVERRIDE="",
                               LIBGL_KOPPER_DRI2="", SELKIES_GPU_DRIVER="",
                               SELKIES_GPU_RENDER_NODE=""),
                    GL_BLOCK_START,
                    'echo "GL=${GALLIUM_DRIVER:-unset} DRIVER=${SELKIES_GPU_DRIVER:-unset}'
                    ' NODE=${SELKIES_GPU_RENDER_NODE:-unset}"')
    return {"zink": "GL=zink" in out["state"], "state": out["state"],
            "said": out["stdout"], "rc": out["rc"]}


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

# --- what the probe resolves that from -----------------------------------
with tempfile.TemporaryDirectory() as fake:
    def node_named(driver: str) -> str:
        """A render node in a stand-in /sys/class/drm whose device runs `driver`."""
        node = f"renderD{128 + len(os.listdir(fake))}"
        device = os.path.join(fake, node, "device")
        os.makedirs(device)
        os.symlink(os.path.join(fake, "drivers", driver), os.path.join(device, "driver"))
        return f"/dev/dri/{node}"

    drivers = {name: node_named(name) for name in ("nvidia", "nvidia-drm", "i915", "nouveau")}
    read = {name: BV.render_node_driver(node, fake) for name, node in drivers.items()}
    res.check("a render node reports the driver behind it",
              read["i915"] == "i915" and read["nouveau"] == "nouveau", read)
    res.check("both spellings of the proprietary stack read as one driver",
              read["nvidia"] == "nvidia" and read["nvidia-drm"] == "nvidia", read)
    res.check("a node with no identity here reports none",
              BV.render_node_driver("/dev/dri/renderD200", fake) == "", "")
    res.check("the node's own driver wins over the devices lying around",
              BV.session_gpu(drivers["i915"], fake, nvidia_device="/dev/null") == "i915")
    res.check("a driver with no render node still answers for itself",
              BV.session_gpu("", fake, nvidia_device="/dev/null") == "nvidia"
              and BV.session_gpu("", fake, nvidia_device="/nonexistent") == "")
    res.check("only the NVIDIA stack asks for Zink",
              BV.gl_environment("nvidia") == BV.ZINK_ENVIRONMENT
              and BV.gl_environment("i915") == {} and BV.gl_environment("nouveau") == {})

# --- the GPU the session renders on, and the GL stack it implies ---------
NVIDIA_REPORT = ('echo SELKIES_GPU_RENDER_NODE=/dev/dri/renderD128; '
                 'echo SELKIES_GPU_DRIVER=nvidia; echo MESA_LOADER_DRIVER_OVERRIDE=zink; '
                 'echo GALLIUM_DRIVER=zink; echo LIBGL_KOPPER_DRI2=1')
INTEL_REPORT = ('echo SELKIES_GPU_RENDER_NODE=/dev/dri/renderD128; '
                'echo SELKIES_GPU_DRIVER=i915')

on_nvidia = gl_verdict(NVIDIA_REPORT)
res.check("a session rendering on NVIDIA runs GL through Zink",
          on_nvidia["zink"] and "DRIVER=nvidia" in on_nvidia["state"], on_nvidia["state"])

on_intel = gl_verdict(INTEL_REPORT)
res.check("a session rendering on another vendor keeps Mesa's own driver",
          not on_intel["zink"] and "DRIVER=i915" in on_intel["state"]
          and "NODE=/dev/dri/renderD128" in on_intel["state"], on_intel["state"])

opted_out = gl_verdict(NVIDIA_REPORT, {"DISABLE_ZINK": "true"})
res.check("DISABLE_ZINK drops the Zink half and keeps the GPU",
          not opted_out["zink"] and "DRIVER=nvidia" in opted_out["state"], opted_out["state"])
res.check("and says the opt-out is what left the GPU behind",
          "DISABLE_ZINK is set" in opted_out["said"], opted_out["said"].strip()[:90])

# The fallback is the old device test, so what it answers depends on this host.
nvidia_here = bool(glob.glob("/dev/nvidia*")) and bool(shutil.which("nvidia-smi"))
no_report = gl_verdict("exit 1")
res.check("a probe with no report falls back to the driver's own devices",
          no_report["zink"] == nvidia_here, f"{no_report['state']} (nvidia here: {nvidia_here})")

# A selkies too old for the question answers the other one, and a backend name
# is not an assignment: exporting it would take the container down at boot.
skewed = gl_verdict("echo wayland")
res.check("an answer that is not a report is not exported",
          skewed["rc"] == 0 and skewed["zink"] == nvidia_here, f"rc={skewed['rc']} {skewed['state']}")

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
