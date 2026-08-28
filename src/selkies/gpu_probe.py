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
import shlex
import sys
from typing import Any, Dict, Tuple

# A container-level knob rather than a server setting: selkies runs whichever
# backend it is handed, and only the entrypoint choosing that backend acts on this.
X11_FALLBACK_ENV: str = "SELKIES_WAYLAND_X11_FALLBACK"

# The environment a container entrypoint records for its services, beside the
# runtime directory it computes.
SESSION_ENV_FILE: str = "container-env"


def session_environment(path: str) -> Dict[str, str]:
    """The variables an entrypoint recorded for the session, from `export K=V` lines.

    The GL stack a session runs on is settled by the entrypoint (a Zink override
    for NVIDIA, and what an unaccelerated verdict then unsets), and those are
    exports of the process tree it launched: a probe run by hand against a live
    container inherits none of them, so without this it reports a stack the
    session does not use.

    Args:
        path: The recorded environment file; missing or unreadable yields nothing.

    Returns:
        The variables it sets, unquoted; lines it cannot parse are skipped.
    """
    found: Dict[str, str] = {}
    try:
        with open(path, encoding="utf-8", errors="replace") as handle:
            lines = handle.read().splitlines()
    except OSError:
        return found
    for line in lines:
        if not line.startswith("export "):
            continue
        name, _, value = line[len("export "):].partition("=")
        if not name.isidentifier():
            continue
        try:
            parsed = shlex.split(value)
        except ValueError:
            continue
        found[name] = parsed[0] if parsed else ""
    return found


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
        return "wayland", "Wayland backend: no GPU in this container; compositing in software"
    reason = report.get("error") or "no hardware acceleration"
    if allow_x11:
        return "x11", (f"Wayland backend: {reason}{where}; falling back to X11 so the "
                       f"session keeps the GPU")
    return "wayland-software", f"Wayland backend: {reason}{where}; compositing in software"


def main() -> int:
    """Probe the GPU via pixelflux and print the recommended backend.

    Returns:
        Process exit code: 0 with the backend on stdout, non-zero (and no
        backend printed) when no report could be obtained: a pixelflux too
        old to carry one, or a driver that refuses to answer at all.
    """
    # What the entrypoint settled for the session, for a run that did not inherit
    # it; never over what this process was actually given.
    recorded = session_environment(
        os.path.join(os.environ.get("XDG_RUNTIME_DIR", ""), SESSION_ENV_FILE))
    for name, value in recorded.items():
        os.environ.setdefault(name, value)
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
    backend, reason = recommend(
        report, parse_bool(os.environ.get(X11_FALLBACK_ENV), default=True))
    print(reason, file=sys.stderr)
    print(backend)
    return 0


if __name__ == "__main__":
    sys.exit(main())
