#!/usr/bin/env python3
"""Which backend the example container starts, given what the GPU probe found.

Two halves, because the decision is made in two places. `selkies-gpu-probe`
weighs a GPU report and names a backend: a Wayland session on a GPU it cannot
reach loses hardware acceleration the same machine would have kept under Xvfb,
while with no GPU at all both backends render in software and moving would trade
a capability for nothing. The entrypoint then acts on that name, and must act
before anything derived from the backend -- the display, the session type and the
toolkit defaults all follow it, so a late switch yields a half-Wayland container.

The second half runs the real entrypoint against a stubbed probe and reads the
environment it hands to the services, rather than restating its logic here.
"""
import os
import shutil
import subprocess
import sys
import tempfile

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
ENTRYPOINT = os.path.join(REPO, "addons", "example", "container-entrypoint.sh")
# The entrypoint hands over to s6 after deriving the service set; everything the
# services read is already in the environment file by then, so the copy under
# test stops at that boundary and never touches /etc/service.
STOP_AT = "# Derive the service set from the environment toggles"

sys.path.insert(0, os.path.join(REPO, "src"))
from selkies.gpu_probe import recommend  # noqa: E402

passed = failed = 0


def check(label, ok, detail=""):
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [wl-fallback] {label}  {detail}", flush=True)


def report(accelerated=False, gpu=False, node="", renderer="", error=""):
    return {"accelerated": accelerated, "gpu": gpu, "node": node,
            "renderer": renderer, "error": error}


def _sh_quote(text):
    return "'" + text.replace("'", "'\\''") + "'"


def run(probe_stdout, probe_rc=0, env=None):
    """Run the entrypoint's environment phase and return what it exported."""
    with tempfile.TemporaryDirectory() as tmp:
        stub_dir = os.path.join(tmp, "bin")
        os.mkdir(stub_dir)
        # The probe's verdict is the decision's only input; what it answers is
        # arranged here so each branch is exercised on demand.
        emit = "printf '%%s\\n' %s\n" % _sh_quote(probe_stdout) if probe_stdout else ""
        probe = os.path.join(stub_dir, "selkies-gpu-probe")
        with open(probe, "w") as f:
            f.write(f"#!/bin/sh\ntouch {tmp}/probe-ran\n{emit}exit {probe_rc}\n")
        os.chmod(probe, 0o755)
        for name in ("sudo-root", "nvidia-smi", "dig"):
            path = os.path.join(stub_dir, name)
            with open(path, "w") as f:
                f.write("#!/bin/sh\nexit 0\n")
            os.chmod(path, 0o755)

        script = os.path.join(tmp, "entrypoint.sh")
        with open(ENTRYPOINT) as f:
            body = f.read()
        if STOP_AT not in body:
            return None, None
        with open(script, "w") as f:
            f.write(body.split(STOP_AT)[0])
        os.chmod(script, 0o755)

        runtime = os.path.join(tmp, "runtime")
        child = dict(os.environ)
        # Cleared so what comes back was set by the entrypoint, not inherited
        # from whatever session the tests happen to run in.
        for name in ("SELKIES_WAYLAND", "PIXELFLUX_WAYLAND", "DISPLAY",
                     "XDG_SESSION_TYPE", "GDK_BACKEND", "MOZ_ENABLE_WAYLAND"):
            child.pop(name, None)
        child.update(env or {})
        child["XDG_RUNTIME_DIR"] = runtime
        child["PATH"] = stub_dir + os.pathsep + os.environ.get("PATH", "")
        proc = subprocess.run(["bash", script], capture_output=True, text=True,
                              env=child, timeout=120)
        exported = {"_stdout": proc.stdout}
        env_file = os.path.join(runtime, "container-env")
        if os.path.exists(env_file):
            with open(env_file) as f:
                for line in f:
                    if line.startswith("export "):
                        name, _, value = line[len("export "):].partition("=")
                        exported[name] = value.strip().strip("'")
        exported["_probe_ran"] = os.path.exists(os.path.join(tmp, "probe-ran"))
        return exported, proc


# --- what the probe recommends -------------------------------------------

backend, why = recommend(report(accelerated=True, gpu=True, node="/dev/dri/renderD128",
                                renderer="NVIDIA RTX 4090"))
check("a reachable GPU argues for Wayland", backend == "wayland", backend)
check("acceleration is reported",
      "hardware acceleration through NVIDIA RTX 4090" in why, why)

backend, why = recommend(report(gpu=True, node="/dev/dri/renderD128",
                                error="Failed to create GBM device"))
check("an unreachable GPU argues for X11", backend == "x11", backend)
check("the reason for the fallback is reported",
      "Failed to create GBM device" in why and "/dev/dri/renderD128" in why, why)

backend, why = recommend(report(gpu=True, node="/dev/dri/renderD128",
                                error="Failed to create GBM device"), allow_x11=False)
check("declining X11 argues for software Wayland", backend == "wayland-software", backend)
check("software rendering is reported", "compositing in software" in why, why)

backend, why = recommend(report(error="No render node"))
check("no GPU argues for Wayland", backend == "wayland", backend)
check("the absent GPU is reported", "no GPU in this container" in why, why)

# --- what the entrypoint does with it ------------------------------------

if not shutil.which("bash"):
    print("SKIP bash not found, so the entrypoint cannot be run", flush=True)
    sys.exit(77)  # helpers.SKIP_EXIT, without importing the e2e helper module

env, proc = run("wayland", env={"SELKIES_WAYLAND": "true", "GALLIUM_DRIVER": "zink"})
if env is None:
    print(f"FAIL  [wl-fallback] entrypoint has the expected structure  missing: {STOP_AT}")
    sys.exit(1)
check("a Wayland verdict keeps Wayland", env.get("SELKIES_WAYLAND") == "true",
      f"{env.get('SELKIES_WAYLAND')} (exit {proc.returncode})")
check("Wayland takes the compositor's display", env.get("DISPLAY") == ":0",
      env.get("DISPLAY"))
check("an accelerated Wayland session keeps hardware GL",
      env.get("GALLIUM_DRIVER") == "zink", env.get("GALLIUM_DRIVER", "(unset)"))

env, _ = run("x11", env={"SELKIES_WAYLAND": "true", "GALLIUM_DRIVER": "zink"})
check("an X11 verdict switches the backend", env.get("SELKIES_WAYLAND") == "false",
      env.get("SELKIES_WAYLAND"))
# The switch has to precede everything derived from the backend, or the container
# runs Xvfb while the session still believes it is on Wayland.
check("the fallback takes the X11 display", env.get("DISPLAY") == ":20",
      env.get("DISPLAY"))
check("the fallback drops the Wayland session type",
      env.get("XDG_SESSION_TYPE", "") != "wayland", env.get("XDG_SESSION_TYPE", "(unset)"))
check("the fallback leaves toolkits on X11",
      "MOZ_ENABLE_WAYLAND" not in env and not env.get("GDK_BACKEND", "").startswith("wayland"),
      f"GDK_BACKEND={env.get('GDK_BACKEND', '(unset)')}")
# Under Xvfb the GPU is still reachable through Vulkan, so the session keeps Zink;
# only the X server itself drops it (services/xvfb/run).
check("the X11 fallback keeps hardware GL for the session",
      env.get("GALLIUM_DRIVER") == "zink", env.get("GALLIUM_DRIVER", "(unset)"))

env, _ = run("wayland-software", env={"SELKIES_WAYLAND": "true",
                                      "GALLIUM_DRIVER": "zink",
                                      "MESA_LOADER_DRIVER_OVERRIDE": "zink"})
check("a software verdict keeps Wayland", env.get("SELKIES_WAYLAND") == "true",
      env.get("SELKIES_WAYLAND"))
check("a software Wayland session gets software GL",
      "GALLIUM_DRIVER" not in env and "MESA_LOADER_DRIVER_OVERRIDE" not in env,
      f"GALLIUM_DRIVER={env.get('GALLIUM_DRIVER', '(unset)')}")

# A probe that cannot answer must not silently change the operator's backend.
env, _ = run("", probe_rc=1, env={"SELKIES_WAYLAND": "true"})
check("a silent probe leaves the backend alone", env.get("SELKIES_WAYLAND") == "true",
      env.get("SELKIES_WAYLAND"))

env, _ = run("wayland", env={"SELKIES_WAYLAND": "false"})
check("X11 is never probed", env.get("_probe_ran") is False)
check("X11 stays X11", env.get("SELKIES_WAYLAND") == "false", env.get("SELKIES_WAYLAND"))

# The legacy alias resolves before the probe, so a Wayland session asked for that
# way is judged the same as one asked for by the current name.
env, _ = run("x11", env={"PIXELFLUX_WAYLAND": "true"})
check("the legacy Wayland toggle is judged too", env.get("SELKIES_WAYLAND") == "false",
      env.get("SELKIES_WAYLAND"))

print(f"[wl-fallback] {passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
