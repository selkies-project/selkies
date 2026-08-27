#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Install the native package in /out and prove the prefix it lays down works
# with nothing but the package's own declared dependencies.
#
# The build container carries a whole toolchain, so a runtime library the
# package forgot to declare is satisfied there and missing on a user's machine.
# This runs in a pristine image of the same distribution instead, and checks
# three things a bare `--help` does not: that every shared object the bundled
# wheels ship resolves, that the libraries the capture stack opens by name at
# runtime (no ELF dependency names them) are present, and that both extension
# modules import.
#
# $INSTALL is the distribution's install command for /out/*, from the packages
# workflow matrix.

set -eu

sh -c "${INSTALL}" > /dev/null

test -d /opt/selkies || {
    echo "::error::the package installed nothing at /opt/selkies"
    exit 1
}

selkies --help > /dev/null
echo "selkies --help runs"

# A wheel's vendored libraries carry auditwheel's hash in their name and sit
# beside each other under $ORIGIN; the loader resolves them at import while
# ldd, invoked from elsewhere, reports them missing. Only names without that
# hash are the system's to provide.
missing="$(find /opt/selkies \( -name '*.so' -o -name '*.so.*' \) | while read -r f; do
    ldd "${f}" 2>/dev/null | grep 'not found' \
        | grep -vE '[a-zA-Z0-9_+]-[0-9a-f]{8}\.so' \
        | sed "s|^|$(basename "${f}"): |"
done | sort -u)"
if [ -n "${missing}" ]; then
    echo "::error::shared objects with unresolved dependencies:"
    echo "${missing}"
    exit 1
fi
echo "every bundled shared object resolves"

/opt/selkies/bin/python3 - <<'PY'
import ctypes
import sys

# Opened by name at runtime: pixelflux reaches for the Wayland server library
# and the GPU stack, and pulsectl for the PulseAudio client, so no ELF
# dependency names any of them and only a declared package dependency puts
# them on the system.
NEEDED = [
    "libwayland-server.so.0",
    "libEGL.so.1",
    "libgbm.so.1",
    "libpulse.so.0",
    "libva.so.2",
    "libdrm.so.2",
    "libxkbcommon.so.0",
    "libpixman-1.so.0",
]
missing = []
for name in NEEDED:
    try:
        ctypes.CDLL(name)
    except OSError as exc:
        missing.append(f"{name}: {exc}")
if missing:
    print("::error::runtime-loaded libraries the package does not pull in:")
    for line in missing:
        print(" ", line)
    sys.exit(1)
print("every runtime-loaded library is present")
PY

/opt/selkies/bin/python3 -c "import pixelflux, pcmflux"
echo "pixelflux and pcmflux import"
