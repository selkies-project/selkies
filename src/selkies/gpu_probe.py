# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Which display backend this machine's GPU argues for, for a container entrypoint.

A Wayland compositor needs a working GBM/EGL stack on a DRM render node; without
one it composites in software and has no dmabuf to hand its clients either, while
the same machine under Xvfb still reaches the GPU. Device paths cannot answer
whether that stack works -- a node can exist with nothing usable behind it, and an
NVIDIA container can expose the driver with no node at all -- so the answer comes
from pixelflux running the compositor's own renderer bring-up.

Prints the backend to start on stdout and the reason on stderr:

    wayland           the compositor reaches the GPU, or there is no GPU to reach
    x11               a GPU is here that the compositor cannot reach
    wayland-software  the same, with the X11 fallback declined: the session has to
                      render in software throughout, GL clients included

Exits non-zero without printing a backend when no report can be had, so a caller
keeps whatever backend it was asked for instead of acting on a guess.
"""

import os
import sys

# A container-level knob rather than a server setting: selkies runs whichever
# backend it is handed, and only the entrypoint choosing that backend acts on this.
X11_FALLBACK_ENV = "SELKIES_WAYLAND_X11_FALLBACK"


def recommend(report, allow_x11=True):
    """The backend a GPU report argues for, and the line explaining it."""
    node = report.get("node") or ""
    where = f" on {node}" if node else ""
    if report.get("accelerated"):
        return "wayland", (f"Wayland backend: hardware acceleration through "
                           f"{report.get('renderer') or 'the GPU'}{where}")
    if not report.get("gpu"):
        return "wayland", "Wayland backend: no GPU in this container; compositing in software"
    reason = report.get("error") or "no hardware acceleration"
    if allow_x11:
        return "x11", (f"Wayland backend: {reason}{where}; falling back to X11 so the "
                       f"session keeps the GPU")
    return "wayland-software", f"Wayland backend: {reason}{where}; compositing in software"


def main():
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
        # A pixelflux too old to carry the report, or a driver that refuses to
        # answer at all: say so and leave the choice to the caller.
        print(f"Wayland backend: GPU support cannot be determined ({e})", file=sys.stderr)
        return 1
    backend, reason = recommend(
        report, parse_bool(os.environ.get(X11_FALLBACK_ENV), default=True))
    print(reason, file=sys.stderr)
    print(backend)
    return 0


if __name__ == "__main__":
    sys.exit(main())
