#!/usr/bin/env python3
"""The GPU report the container reads before choosing a backend.

pixelflux owns hardware detection, and `selkies gpu-probe` reports what it
finds: whether a Wayland session here would be hardware accelerated, on which
node, and whether a GPU is present at all, as KEY=VALUE lines the container's
entrypoint exports and one summary line on stderr. The container starts X11
instead of Wayland when the answer is no but a GPU is present, so that decision
is only as good as this report: a renamed key or a changed vocabulary leaves the
container on whatever backend was asked for, and a session silently loses the
GPU it was provisioned with. So the report's shape is pinned here, along with
the answers that do not depend on what hardware the test machine has.
"""
import os
import subprocess
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402

SERVER_SIDE = ("SELKIES_GPU_RENDER_NODE", "SELKIES_GPU_DRIVER", "SELKIES_GPU_PRESENT", "SELKIES_GPU_ACCELERATED")


def probe(**env: str) -> tuple:
    """The report `selkies gpu-probe` prints with `env` added: its facts and its summary line."""
    done = subprocess.run([H.SERVER, "gpu-probe"], capture_output=True, text=True, timeout=180,
                          env=dict(os.environ, **env))
    facts = dict(line.split("=", 1) for line in done.stdout.splitlines() if "=" in line)
    summary = next((line for line in done.stderr.splitlines() if line.startswith("GPU: ")), "")
    return facts, summary


def main() -> bool:
    """Pin the gpu-probe report's shape and its hardware-independent answers."""
    res = H.Results("gpu-probe")
    facts, summary = probe()
    res.check("the report has every fact the container reads",
              all(k in facts for k in SERVER_SIDE), sorted(facts))
    res.check("presence and acceleration are answered true or false",
              facts.get("SELKIES_GPU_PRESENT") in ("true", "false")
              and facts.get("SELKIES_GPU_ACCELERATED") in ("true", "false"), facts)

    # Acceleration and its explanation cannot disagree, whichever way this
    # machine answers: the container prints the summary and acts on the flag.
    accelerated = facts.get("SELKIES_GPU_ACCELERATED") == "true"
    res.check("an accelerated report names a node and gives no reason against it",
              not accelerated or (bool(facts.get("SELKIES_GPU_RENDER_NODE")) and "unusable" not in summary),
              (facts, summary))
    res.check("an unaccelerated report says why",
              accelerated or "unusable: " in summary or summary == "GPU: none here", summary)

    # A node that cannot back a renderer is unaccelerated however it fails, and
    # saying so is the whole point: /dev/null opens and makes a GBM device on
    # some drivers, so only carrying the bring-up through tells them apart. Nor
    # does the probe fall back to another node when it is given one.
    for path in ("/dev/dri/renderD999", "/dev/null"):
        bogus, said = probe(DRINODE=path)
        res.check(f"{path} is reported unaccelerated, with why",
                  bogus.get("SELKIES_GPU_ACCELERATED") == "false"
                  and ("unusable: " in said or said == "GPU: none here"), said)
        res.check(f"{path} names no other node",
                  bogus.get("SELKIES_GPU_RENDER_NODE") in ("", path), bogus.get("SELKIES_GPU_RENDER_NODE"))

    # GPU presence is independent of whether the compositor can use one, which
    # is what lets the container tell "no GPU here" from "GPU it cannot reach".
    res.check("GPU presence is reported separately from acceleration",
              facts.get("SELKIES_GPU_PRESENT") == "true" or not accelerated, facts)

    # The recorded environment is the exec shell's to source, not the probe's
    # to read: the probe answers for exactly what it is given, so a variable
    # sitting in the recording alone steers nothing, and one in the actual
    # environment does.
    with tempfile.TemporaryDirectory() as runtime:
        with open(os.path.join(runtime, "container-env"), "w") as fh:
            fh.write("export DRINODE=/dev/dri/renderD999\n")
        env = dict(os.environ, XDG_RUNTIME_DIR=runtime)
        recorded = subprocess.run([H.SERVER, "gpu-probe"],
                                  capture_output=True, text=True, timeout=120, env=env)
        res.check("a recorded variable alone does not steer the probe",
                  "renderD999" not in recorded.stderr, recorded.stderr.strip()[:120])
        given = subprocess.run([H.SERVER, "gpu-probe"],
                               capture_output=True, text=True, timeout=120,
                               env=dict(env, DRINODE="/dev/null"))
        res.check("what the probe is actually given wins",
                  "/dev/null" in given.stderr, given.stderr.strip()[:120])

    # The client-path facts, measured on whatever this machine has: every one
    # reported, each from its own small vocabulary, and consistent with each
    # other, since the entrypoint maps them without looking further.
    client = ("SELKIES_GPU_GL_VENDOR", "SELKIES_GPU_EGL_X11",
              "SELKIES_GPU_MESA_DRIVER", "SELKIES_GPU_VULKAN_PRESENTS")
    res.check("every client-path fact is reported", all(k in facts for k in client), sorted(facts))
    res.check("Mesa's route is one of native, zink, or software",
              facts.get("SELKIES_GPU_MESA_DRIVER") in ("native", "zink", ""), facts)
    res.check("whether Vulkan presents is answered only where Zink is Mesa's route",
              (facts.get("SELKIES_GPU_VULKAN_PRESENTS") in ("true", "false")) == (facts.get("SELKIES_GPU_MESA_DRIVER") == "zink")
              or facts.get("SELKIES_GPU_VULKAN_PRESENTS") == "", facts)
    res.check("a GL vendor is only named where a GPU driver was found",
              not facts.get("SELKIES_GPU_GL_VENDOR") or facts.get("SELKIES_GPU_DRIVER"), facts)
    res.check("Mesa's native route names Mesa as the vendor",
              facts.get("SELKIES_GPU_MESA_DRIVER") != "native" or facts.get("SELKIES_GPU_GL_VENDOR") == "mesa", facts)
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
