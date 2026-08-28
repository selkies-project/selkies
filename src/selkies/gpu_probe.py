# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""What GPU a session has here, for whatever brings one up.

A Wayland compositor needs a working GBM/EGL stack on a DRM render node, and
device paths cannot answer whether that stack works -- a node can exist with
nothing usable behind it, and a driver can be installed with no node at all --
so the answer comes from pixelflux running the compositor's own renderer
bring-up. That bring-up also resolves `render_dri` and the `auto_gpu` token
exactly as the capture will, so the GPU it reports is the one the session uses.

Prints what it found as `KEY=VALUE` lines on stdout, for a caller to export and
act on, and one line saying it in words on stderr:

    SELKIES_GPU_RENDER_NODE  the node the session renders on, empty for none
    SELKIES_GPU_DRIVER       the driver behind it, empty when no GPU backs it
    SELKIES_GPU_PRESENT      whether a GPU is here at all
    SELKIES_GPU_ACCELERATED  whether the compositor's renderer came up on it

What to do about that -- which display backend to start, which GL stack the
session's applications are given -- is the deployment's own policy, and belongs
where the deployment is assembled rather than here.

Exits non-zero without printing anything when no report can be had: a pixelflux
too old to carry one, or a driver that refuses to answer at all.
"""

import os
import sys
from typing import Dict

# The proprietary NVIDIA stack, which no Mesa driver covers; what a session does
# about that is the deployment's policy, not this module's.
NVIDIA_DRIVER: str = "nvidia"


def render_node_driver(node: str, sysfs: str = "/sys/class/drm") -> str:
    """The kernel driver behind a DRM render node.

    Args:
        node: A `/dev/dri/renderD*` path.
        sysfs: Where the DRM class lives; a test points this elsewhere.

    Returns:
        The driver name (`nvidia`, `i915`, `xe`, `amdgpu`, ...), or "" when the
        node has no identity here — a system without `/sys` among others.
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
    """The driver of the GPU the session renders on.

    A render node names its own driver. With no node the GPU can still be
    reachable — the NVIDIA driver and its Vulkan ICD work without a DRM node —
    so the driver device answers for that case alone.

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


def facts(report: Dict[str, object]) -> Dict[str, str]:
    """The GPU a `pixelflux.probe_wayland_gpu` report describes.

    Args:
        report: That report (keys: `accelerated`, `gpu`, `renderer`, `node`,
            `error`).

    Returns:
        The variables a caller exports, all present and all strings.
    """
    node = str(report.get("node") or "")
    # The report echoes the node it was asked for, which a failed bring-up may
    # say is not there at all; that is no node to render on.
    if node and not os.path.exists(node):
        node = ""
    return {
        "SELKIES_GPU_RENDER_NODE": node,
        "SELKIES_GPU_DRIVER": session_gpu(node),
        "SELKIES_GPU_PRESENT": "true" if report.get("gpu") else "false",
        "SELKIES_GPU_ACCELERATED": "true" if report.get("accelerated") else "false",
    }


def summary(report: Dict[str, object], found: Dict[str, str]) -> str:
    """One line describing `found`, for a log.

    Args:
        report: The report it came from, for the renderer name and the error.
        found: The variables `facts` derived.

    Returns:
        The line to print.
    """
    node = found["SELKIES_GPU_RENDER_NODE"]
    where = f" on {node}" if node else ""
    if found["SELKIES_GPU_ACCELERATED"] == "true":
        return f"GPU: {report.get('renderer') or found['SELKIES_GPU_DRIVER']}{where}"
    why = str(report.get("error") or "no hardware acceleration")
    if found["SELKIES_GPU_PRESENT"] == "true":
        return f"GPU: {found['SELKIES_GPU_DRIVER'] or 'unknown'}{where}, unusable: {why}"
    return "GPU: none here"


def main() -> int:
    """Probe the GPU via pixelflux and print what it found.

    Returns:
        Process exit code: 0 with the facts on stdout, non-zero (and nothing
        printed) when no report could be obtained.
    """
    # Imported here so the module stays usable where pixelflux is absent, and so
    # the settings parser never sees this tool's own invocation.
    from .settings import settings
    try:
        import pixelflux
        report = pixelflux.probe_wayland_gpu(
            str(getattr(settings, "render_dri", "") or ""),
            str(getattr(settings, "auto_gpu", "") or ""),
        )
    except Exception as e:
        print(f"GPU: support cannot be determined ({e})", file=sys.stderr)
        return 1
    found = facts(report)
    print(summary(report, found), file=sys.stderr)
    for name, value in found.items():
        print(f"{name}={value}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
