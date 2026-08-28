#!/usr/bin/env python3
"""The GPU report the container reads before choosing a backend.

pixelflux owns hardware detection, so the example container asks it whether a
Wayland session here would be hardware accelerated and starts X11 instead when
the answer is no but a GPU is present. That decision is only as good as this
report: a renamed key or a changed type makes the probe raise, the container
keeps whatever backend was asked for, and a session silently loses the GPU it
was provisioned with. So the shape is pinned here, along with the answers that
do not depend on what hardware the test machine has.
"""
import os
import subprocess
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402

KEYS: dict[str, type] = {"node": str, "accelerated": bool, "gpu": bool, "renderer": str, "error": str}


def main() -> bool:
    """Pin the probe_wayland_gpu report shape and its hardware-independent answers."""
    res = H.Results("gpu-probe")
    try:
        import pixelflux
    except ImportError:
        print("SKIP pixelflux is not installed, so the GPU report cannot be read",
              flush=True)
        sys.exit(H.SKIP_EXIT)

    if not hasattr(pixelflux, "probe_wayland_gpu"):
        # A released pixelflux that predates the report is not a broken contract:
        # the container's probe raises, is caught, and the backend stays as asked.
        res.skip("pixelflux reports on Wayland acceleration",
                 "the installed pixelflux does not carry probe_wayland_gpu yet")
        return res.summary()
    res.check("pixelflux reports on Wayland acceleration", True)

    report = pixelflux.probe_wayland_gpu("", "true")
    res.check("the report has every field the container reads",
              set(report) >= set(KEYS), sorted(report))
    res.check("each field has the type the container expects",
              all(isinstance(report.get(k), t) for k, t in KEYS.items()),
              {k: type(report.get(k)).__name__ for k in KEYS})

    # Acceleration and its explanation cannot disagree, whichever way this
    # machine answers: the container prints the error and acts on the flag.
    res.check("an accelerated report carries no error and names a node",
              not report["accelerated"] or (not report["error"] and report["node"]),
              report)
    res.check("an unaccelerated report says why",
              report["accelerated"] or bool(report["error"]), report)

    # A node that cannot back a renderer is unaccelerated however it fails, and
    # saying so is the whole point: /dev/null opens and makes a GBM device on
    # some drivers, so only carrying the bring-up through tells them apart.
    for path in ("/dev/dri/renderD999", "/dev/null"):
        bogus = pixelflux.probe_wayland_gpu(path, "")
        res.check(f"{path} is reported unaccelerated",
                  not bogus["accelerated"] and bool(bogus["error"]),
                  bogus.get("error"))
        res.check(f"{path} is echoed back as the node tried",
                  bogus["node"] == path, bogus.get("node"))

    # GPU presence is independent of whether the compositor can use one, which
    # is what lets the container tell "no GPU here" from "GPU it cannot reach".
    res.check("GPU presence is reported separately from acceleration",
              report["gpu"] or not report["accelerated"], report)

    # Run by hand against a live container the probe inherits none of the
    # session's environment, and would answer for a GPU stack the session does
    # not use. The recorded environment is what it reads instead.
    repo = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    with tempfile.TemporaryDirectory() as runtime:
        with open(os.path.join(runtime, "container-env"), "w") as fh:
            fh.write("export DRINODE=/dev/dri/renderD999\n")
        env = dict(os.environ, XDG_RUNTIME_DIR=runtime,
                   PYTHONPATH=os.path.join(repo, "src"))
        recorded = subprocess.run([sys.executable, "-m", "selkies.gpu_probe"],
                                  capture_output=True, text=True, timeout=120, env=env)
        res.check("a probe run by hand reads the session's recorded environment",
                  "/dev/dri/renderD999" in recorded.stderr, recorded.stderr.strip()[:120])
        given = subprocess.run([sys.executable, "-m", "selkies.gpu_probe"],
                               capture_output=True, text=True, timeout=120,
                               env=dict(env, DRINODE="/dev/null"))
        res.check("what the probe is actually given still wins",
                  "/dev/null" in given.stderr, given.stderr.strip()[:120])
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
