#!/usr/bin/env python3
"""What the container makes of the GPU its probe reports.

`selkies-gpu-probe` reports one thing: the GPU this session has, and whether the
compositor's renderer came up on it. Every decision downstream is the image's
own, made here in the entrypoint, and each has a way to be wrong: a GL stack
chosen from the devices lying around rather than from the GPU the session
renders on hands a hybrid host Zink for a card it never touches; a backend
chosen without the report starts a compositor that cannot paint; and a bring-up
that dies rather than answering is the one the session is about to attempt, so
starting it anyway is a supervisor restarting a crash with no session to show
for it.

The deciding blocks run here verbatim out of the entrypoint, against a stand-in
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

from selkies import gpu_probe as GP  # noqa: E402

FACTS_BLOCK = 'gpu_status=0\ngpu_facts="$(timeout 60 selkies-gpu-probe)"'
GL_BLOCK = 'if [ "${SELKIES_GPU_DRIVER-}" != "nvidia" ]; then'
BACKEND_BLOCK = 'if [ "${SELKIES_WAYLAND}" = "true" ] && [ "${SELKIES_GPU_PRESENT-}" = "true" ]'
STATE = ('echo "WAYLAND=${SELKIES_WAYLAND:-unset} GL=${GALLIUM_DRIVER:-unset}'
         ' DRIVER=${SELKIES_GPU_DRIVER:-unset} NODE=${SELKIES_GPU_RENDER_NODE:-unset}'
         ' ACCEL=${SELKIES_GPU_ACCELERATED:-unset} PRESENT=${SELKIES_GPU_PRESENT:-unset}"')

NVIDIA_FACTS = ('echo SELKIES_GPU_RENDER_NODE=/dev/dri/renderD128; echo SELKIES_GPU_DRIVER=nvidia; '
                'echo SELKIES_GPU_PRESENT=true; echo SELKIES_GPU_ACCELERATED=true')
INTEL_FACTS = ('echo SELKIES_GPU_RENDER_NODE=/dev/dri/renderD128; echo SELKIES_GPU_DRIVER=i915; '
               'echo SELKIES_GPU_PRESENT=true; echo SELKIES_GPU_ACCELERATED=true')
UNREACHABLE_FACTS = ('echo SELKIES_GPU_RENDER_NODE=; echo SELKIES_GPU_DRIVER=nvidia; '
                     'echo SELKIES_GPU_PRESENT=true; echo SELKIES_GPU_ACCELERATED=false')
NO_GPU_FACTS = ('echo SELKIES_GPU_RENDER_NODE=; echo SELKIES_GPU_DRIVER=; '
                'echo SELKIES_GPU_PRESENT=false; echo SELKIES_GPU_ACCELERATED=false')


def entrypoint_blocks(*starts: str) -> str:
    """The named deciding blocks and the parsing helpers they call, verbatim.

    Args:
        *starts: First line of each block to extract, in the order to run them.
    """
    body = open(ENTRYPOINT, encoding="utf-8").read()
    parts = []
    for name in ("setting_value() {", "is_true() {"):
        begin = body.index(name)
        parts.append(body[begin:body.index("\n}\n", begin) + 3])
    for start in starts:
        begin = body.index(start)
        # Blocks are separated by a blank line and carry none inside them.
        parts.append(body[begin:body.index("\n\n", begin)])
    return "\n".join(parts)


def run(stub: str = "exit 1", env: Optional[dict] = None,
        blocks: tuple = (FACTS_BLOCK, GL_BLOCK, BACKEND_BLOCK)) -> dict:
    """Run the entrypoint's GPU blocks with `stub` standing in for the probe.

    Args:
        stub: Shell body of the stand-in `selkies-gpu-probe`.
        env: Environment the blocks start from.
        blocks: Which blocks to run, in order.

    Returns:
        `state` (the settled variables), `said` (everything printed) and `rc`.
    """
    with tempfile.TemporaryDirectory() as tmp:
        probe = os.path.join(tmp, "selkies-gpu-probe")
        with open(probe, "w") as fh:
            fh.write("#!/bin/bash\n" + stub + "\n")
        os.chmod(probe, 0o755)
        script = os.path.join(tmp, "blocks.sh")
        with open(script, "w") as fh:
            fh.write("set -e\n" + entrypoint_blocks(*blocks) + "\n" + STATE + "\n")
        out = subprocess.run(
            ["bash", script], capture_output=True, text=True, timeout=120,
            env=dict(os.environ, PATH=tmp + os.pathsep + os.environ["PATH"],
                     GALLIUM_DRIVER="", MESA_LOADER_DRIVER_OVERRIDE="", LIBGL_KOPPER_DRI2="",
                     SELKIES_GPU_DRIVER="", SELKIES_GPU_RENDER_NODE="",
                     SELKIES_GPU_PRESENT="", SELKIES_GPU_ACCELERATED="",
                     **(env or {})))
        lines = [ln for ln in out.stdout.strip().splitlines() if ln]
        return {"state": lines[-1] if lines else "", "said": out.stdout, "rc": out.returncode}


res = H.Results("backend-verdict")
WAYLAND = {"SELKIES_WAYLAND": "true"}

# --- what the probe reports, and what the entrypoint does when it cannot ---
reported = run(NVIDIA_FACTS, WAYLAND)
res.check("the GPU the probe reports is the one the session carries",
          "DRIVER=nvidia" in reported["state"] and "NODE=/dev/dri/renderD128" in reported["state"]
          and "ACCEL=true" in reported["state"], reported["state"])

skewed = run("echo wayland", WAYLAND)
res.check("an answer that is not a report is not exported",
          skewed["rc"] == 0 and "NODE=unset" in skewed["state"], skewed["state"])

crashed = run("kill -SEGV $$", WAYLAND)
res.check("a bring-up that dies counts as a GPU the compositor cannot reach",
          "PRESENT=true" in crashed["state"] and "ACCEL=false" in crashed["state"],
          crashed["state"])
res.check("and says so", "did not survive" in crashed["said"], crashed["said"].strip()[:110])

# The last resort is the device-node test, so what it answers depends on this host.
nvidia_here = bool(glob.glob("/dev/nvidia*")) and bool(shutil.which("nvidia-smi"))
silent = run("exit 1", WAYLAND)
res.check("no report at all falls back to the driver's own devices",
          ("DRIVER=nvidia" in silent["state"]) == nvidia_here,
          f"{silent['state']} (nvidia here: {nvidia_here})")
res.check("and leaves the backend as it was asked for",
          "WAYLAND=true" in silent["state"], silent["state"])

# --- the GL stack, which follows that GPU ---------------------------------
res.check("a session rendering on NVIDIA runs GL through Zink",
          "GL=zink" in run(NVIDIA_FACTS, WAYLAND)["state"])
res.check("a session rendering on another vendor keeps Mesa's own driver",
          "GL=unset" in run(INTEL_FACTS, WAYLAND)["state"])
opted_out = run(NVIDIA_FACTS, dict(WAYLAND, DISABLE_ZINK="true"))
res.check("DISABLE_ZINK drops the Zink half and keeps the GPU",
          "GL=unset" in opted_out["state"] and "DRIVER=nvidia" in opted_out["state"],
          opted_out["state"])
res.check("and says the opt-out is what left the GPU behind",
          "DISABLE_ZINK is set" in opted_out["said"], opted_out["said"].strip()[:90])

# --- the backend, which follows it too ------------------------------------
res.check("a GPU the compositor reached keeps the session on Wayland",
          "WAYLAND=true" in run(NVIDIA_FACTS, WAYLAND)["state"])
unreachable = run(UNREACHABLE_FACTS, WAYLAND)
res.check("a GPU it cannot reach moves the session to X11, which still has it",
          "WAYLAND=false" in unreachable["state"] and "GL=zink" in unreachable["state"],
          unreachable["state"])
res.check("and says why", "cannot reach it" in unreachable["said"],
          unreachable["said"].strip()[:110])
software = run(UNREACHABLE_FACTS, dict(WAYLAND, SELKIES_WAYLAND_X11_FALLBACK="false"))
res.check("a session that declined X11 composites in software, GL included",
          "WAYLAND=true" in software["state"] and "GL=unset" in software["state"],
          software["state"])
res.check("no GPU at all leaves the session where it is",
          "WAYLAND=true" in run(NO_GPU_FACTS, WAYLAND)["state"])
res.check("an X11 session is never moved",
          "WAYLAND=false" in run(UNREACHABLE_FACTS, {"SELKIES_WAYLAND": "false"})["state"])

# --- what the probe resolves the GPU from ---------------------------------
with tempfile.TemporaryDirectory() as fake:
    def node_named(driver: str) -> str:
        """A render node in a stand-in /sys/class/drm whose device runs `driver`."""
        node = f"renderD{128 + len(os.listdir(fake))}"
        device = os.path.join(fake, node, "device")
        os.makedirs(device)
        os.symlink(os.path.join(fake, "drivers", driver), os.path.join(device, "driver"))
        return f"/dev/dri/{node}"

    drivers = {name: node_named(name) for name in ("nvidia", "nvidia-drm", "i915", "nouveau")}
    read = {name: GP.render_node_driver(node, fake) for name, node in drivers.items()}
    res.check("a render node reports the driver behind it",
              read["i915"] == "i915" and read["nouveau"] == "nouveau", read)
    res.check("both spellings of the proprietary stack read as one driver",
              read["nvidia"] == "nvidia" and read["nvidia-drm"] == "nvidia", read)
    res.check("a node with no identity here reports none",
              GP.render_node_driver("/dev/dri/renderD200", fake) == "")
    res.check("the node's own driver wins over the devices lying around",
              GP.session_gpu(drivers["i915"], fake, nvidia_device="/dev/null") == "i915")
    res.check("a driver with no render node still answers for itself",
              GP.session_gpu("", fake, nvidia_device="/dev/null") == "nvidia"
              and GP.session_gpu("", fake, nvidia_device="/nonexistent") == "")

missing = GP.facts({"node": "/dev/dri/renderD999", "gpu": True, "accelerated": False,
                    "error": "Failed to open render device"})
res.check("a node the report says is not there is no node to render on",
          missing["SELKIES_GPU_RENDER_NODE"] == "" and missing["SELKIES_GPU_PRESENT"] == "true"
          and missing["SELKIES_GPU_ACCELERATED"] == "false", missing)
res.check("every fact is reported whatever the report holds",
          set(GP.facts({})) == {"SELKIES_GPU_RENDER_NODE", "SELKIES_GPU_DRIVER",
                                "SELKIES_GPU_PRESENT", "SELKIES_GPU_ACCELERATED"},
          GP.facts({}))

sys.exit(0 if res.summary() else 1)
