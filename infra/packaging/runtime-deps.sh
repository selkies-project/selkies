#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Print the distribution packages the server needs at run time, one per line,
# as this distribution's package manager names them.
#
# The list is read off the build rather than written down per distribution:
# every library the binary links, as the loader resolves it here, and the
# libraries pixelflux and pcmflux open by name at run time, which no ELF
# dependency names, each traced to the package that installed it. A library
# no package owns was built for this package and has no business being linked
# dynamically, so it fails the build. The runtime-loaded list is the one the
# base image and scripts/ci/verify-package.sh check, which
# tests/unit/test_package_deps.py holds them all to.
#
# Usage: runtime-deps.sh <binary>
set -eu
BINARY="${1:?usage: runtime-deps.sh <binary>}"
RUNTIME_LOADED="libwayland-server.so.0 libEGL.so.1 libgbm.so.1 libpulse.so.0 libva.so.2 libva-drm.so.2 libdrm.so.2 libxkbcommon.so.0 libpixman-1.so.0"

# Where the loader would find a library by soname alone.
locate() {
    for dir in /lib /lib64 /usr/lib /usr/lib64 /usr/lib/*-linux-gnu /lib/*-linux-gnu; do
        if [ -e "${dir}/$1" ]; then
            echo "${dir}/$1"
            return 0
        fi
    done
    return 1
}

# The package that installed `path`. A package lists a file under the path it
# shipped it at, which can be either side of a /lib to /usr/lib merge or the
# target of the symlink the loader followed.
owner() {
    real="$(readlink -f "$1")"
    for path in "${real}" "$1" "/usr${real}" "${real#/usr}"; do
        if command -v dpkg-query > /dev/null; then
            found="$(dpkg -S "${path}" 2>/dev/null | grep -v '^diversion ' | head -1 | sed 's/: .*//; s/:[^,]*//g; s/,.*//')"
        elif command -v apk > /dev/null; then
            found="$(apk info --who-owns "${path}" 2>/dev/null | sed -n 's/.* is owned by //p' | sed -E 's/-[^-]+-r[0-9]+$//')"
        elif command -v pacman > /dev/null; then
            found="$(pacman -Qqo "${path}" 2>/dev/null || true)"
        else
            found="$(rpm -qf --qf '%{NAME}\n' "${path}" 2>/dev/null || true)"
        fi
        if [ -n "${found}" ]; then
            echo "${found}"
            return 0
        fi
    done
    echo "runtime-deps: no package owns ${1}" >&2
    return 1
}

linked="$(ldd "${BINARY}")"
if printf '%s\n' "${linked}" | grep 'not found' >&2; then
    echo "runtime-deps: ${BINARY} links libraries this system lacks" >&2
    exit 1
fi
libs="$(printf '%s\n' "${linked}" | awk '$2 == "=>" && $3 ~ /^\// { print $3 }')"
[ -n "${libs}" ] || { echo "runtime-deps: ldd resolved nothing for ${BINARY}" >&2; exit 1; }
for name in ${RUNTIME_LOADED}; do
    found="$(locate "${name}")" || { echo "runtime-deps: ${name} is not installed here" >&2; exit 1; }
    libs="${libs} ${found}"
done
packages=""
for lib in ${libs}; do
    packages="${packages} $(owner "${lib}")"
done
# shellcheck disable=SC2086  # package names never contain spaces
printf '%s\n' ${packages} | sort -u
