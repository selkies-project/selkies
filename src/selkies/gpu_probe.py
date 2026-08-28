# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""What this machine's GPU means for a session, for whatever brings one up.

A Wayland compositor needs a working GBM/EGL stack on a DRM render node; without
one it composites in software and has no dmabuf to hand its clients either, while
the same machine under Xvfb still reaches the GPU. Device paths cannot answer
whether that stack works -- a node can exist with nothing usable behind it, and a
driver can be installed with no node at all -- so the answer comes from pixelflux
running the compositor's own renderer bring-up.

That bring-up also settles WHICH GPU the session uses, since it resolves
`render_dri` and the `auto_gpu` token exactly as the capture does. Everything
downstream has to follow that one answer: the GL stack the session's applications
run on, and the render node the X11 framebuffer hands its clients. Deciding any
of them from device paths instead ("an NVIDIA device exists, so Zink") points a
session at a GPU it does not render on, which is a hybrid host's whole failure
mode.

Prints the backend to start on stdout and the reason on stderr:

    wayland           the compositor reaches the GPU, or there is no GPU to reach
    x11               a GPU is here that the compositor cannot reach
    wayland-software  the same, with the X11 fallback declined: the session has to
                      render in software throughout, GL clients included

With `--session-env`, prints that GPU and the environment it implies as
`KEY=VALUE` lines instead, for the caller to export.

Exits non-zero without printing anything when no report can be had, so a caller
keeps whatever backend it was asked for instead of acting on a guess.
"""

import os
import sys
from typing import Any, Dict, Tuple

# A deployment-level knob rather than a server setting: selkies runs whichever
# backend it is handed, and only whoever chooses that backend acts on this.
X11_FALLBACK_ENV: str = "SELKIES_WAYLAND_X11_FALLBACK"

# Mesa carries no driver for the proprietary NVIDIA stack, so GL there runs
# through Zink on its Vulkan driver; every other vendor has a native one.
NVIDIA_DRIVER: str = "nvidia"
ZINK_ENVIRONMENT: Dict[str, str] = {
    "MESA_LOADER_DRIVER_OVERRIDE": "zink",
    "GALLIUM_DRIVER": "zink",
    "LIBGL_KOPPER_DRI2": "1",
}


def render_node_driver(node: str, sysfs: str = "/sys/class/drm") -> str:
    """The kernel driver behind a DRM render node.

    Args:
        node: A `/dev/dri/renderD*` path.
        sysfs: Where the DRM class lives; a test points this elsewhere.

    Returns:
        The driver name (`nvidia`, `i915`, `xe`, `amdgpu`, ...), or "" when the
        node has no identity here — a container without `/sys` among others.
    """
    if not node:
        return ""
    link = os.path.join(sysfs, os.path.basename(node), "device", "driver")
    if not os.path.islink(link):
        return ""
    driver = os.path.basename(os.path.realpath(link))
    # The DRM node of the proprietary stack answers `nvidia` or `nvidia-drm`
    # depending on the kernel; nouveau is a different driver and stays itself.
    return NVIDIA_DRIVER if driver.startswith(NVIDIA_DRIVER) else driver


def session_gpu(node: str, sysfs: str = "/sys/class/drm",
                nvidia_device: str = "/dev/nvidiactl") -> str:
    """The driver the session's GL stack has to match.

    A render node names its own driver. With no node the GPU can still be
    reachable — the NVIDIA container runtime injects the driver and its Vulkan
    ICD without a DRM node, which is how Zink works there — so the driver device
    answers for that case alone.

    Args:
        node: The render node the session renders on, or "" for none.
        sysfs: Where the DRM class lives; a test points this elsewhere.
        nvidia_device: The NVIDIA control device; a test points this elsewhere.

    Returns:
        The driver name, or "" when no GPU backs the session.
    """
    driver = render_node_driver(node, sysfs)
    if driver:
        return driver
    return NVIDIA_DRIVER if os.path.exists(nvidia_device) else ""


def gl_environment(driver: str) -> Dict[str, str]:
    """The GL environment a session on `driver` needs, empty where Mesa's own
    driver is the right one.

    Args:
        driver: The driver from `session_gpu`.

    Returns:
        The variables to export.
    """
    return dict(ZINK_ENVIRONMENT) if driver == NVIDIA_DRIVER else {}


def recommend(report: Dict[str, Any], allow_x11: bool = True) -> Tuple[str, str]:
    """The backend a GPU report argues for, and the line explaining it.

    Args:
        report: A `pixelflux.probe_wayland_gpu` report (keys: `accelerated`,
            `gpu`, `renderer`, `node`, `error`).
        allow_x11: Whether an unaccelerated GPU may fall back to X11; when
            False the session stays on Wayland and composites in software.

    Returns:
        A `(backend, reason)` pair: the backend name to print on stdout and
        the human-readable explanation for stderr.
    """
    node = report.get("node") or ""
    where = f" on {node}" if node else ""
    if report.get("accelerated"):
        return "wayland", (f"Wayland backend: hardware acceleration through "
                           f"{report.get('renderer') or 'the GPU'}{where}")
    if not report.get("gpu"):
        return "wayland", "Wayland backend: no GPU here; compositing in software"
    reason = report.get("error") or "no hardware acceleration"
    if allow_x11:
        return "x11", (f"Wayland backend: {reason}{where}; falling back to X11 so the "
                       f"session keeps the GPU")
    return "wayland-software", f"Wayland backend: {reason}{where}; compositing in software"


def main() -> int:
    """Probe the GPU via pixelflux and print what the caller has to act on.

    With no arguments that is the recommended backend; with `--session-env` it
    is the GPU the session renders on and the environment that GPU implies,
    as `KEY=VALUE` lines.

    Returns:
        Process exit code: 0 with the answer on stdout, non-zero (and nothing
        printed) when no report could be obtained: a pixelflux too old to carry
        one, or a driver that refuses to answer at all.
    """
    wants_session_env = "--session-env" in sys.argv[1:]
    # The settings parser reads the same argv and logs anything it does not know.
    sys.argv = [arg for arg in sys.argv if arg != "--session-env"]
    # Imported here so the module stays usable where pixelflux is absent, and so
    # the settings parser never sees this tool's own invocation.
    from .settings import parse_bool, settings
    try:
        import pixelflux
        report = pixelflux.probe_wayland_gpu(
            str(getattr(settings, "render_dri", "") or ""),
            str(getattr(settings, "auto_gpu", "") or ""),
        )
    except Exception as e:
        print(f"Wayland backend: GPU support cannot be determined ({e})", file=sys.stderr)
        return 1
    if wants_session_env:
        node = report.get("node") or ""
        driver = session_gpu(node)
        where = f" on {node}" if node else ""
        if driver == NVIDIA_DRIVER:
            print(f"OpenGL runs through Zink on the NVIDIA Vulkan driver{where}", file=sys.stderr)
        elif driver:
            print(f"OpenGL renders through Mesa's {driver} driver{where}", file=sys.stderr)
        else:
            print("No GPU the session can render on; OpenGL is software", file=sys.stderr)
        answer = {"SELKIES_GPU_RENDER_NODE": node, "SELKIES_GPU_DRIVER": driver}
        answer.update(gl_environment(driver))
        for name, value in answer.items():
            print(f"{name}={value}")
        return 0

    backend, reason = recommend(
        report, parse_bool(os.environ.get(X11_FALLBACK_ENV), default=True))
    print(reason, file=sys.stderr)
    print(backend)
    return 0


if __name__ == "__main__":
    sys.exit(main())
