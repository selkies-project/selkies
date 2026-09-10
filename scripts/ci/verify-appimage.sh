#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Prove the AppImage in $1 works somewhere other than where it was built.
#
# The build's own `--help` smoke test runs inside the AppDir at the path the
# conda prefix was installed to, where an entry point that names that path
# resolves anyway. What ships mounts at a path chosen at run time, and every
# absolute path the build left inside is wrong there: the interpreter each
# entry point names, and the module directory and startup script the bundled
# sound server has compiled in. This runs from an extracted copy instead,
# which is also how a user reaches the payload by hand.
#
# Every path checked has to be inside that copy rather than merely resolve,
# because the build path is still there on the machine that built the AppImage.
#
# Usage: verify-appimage.sh <appimage>

set -eu

IMAGE="$(readlink -f "${1:?usage: verify-appimage.sh <appimage>}")"
WORK="$(mktemp -d)"
# A removal racing the sound server's own teardown is not what this reports on
trap 'rm -rf "${WORK}" 2>/dev/null || true' EXIT
cd "${WORK}"
"${IMAGE}" --appimage-extract > /dev/null
APP="${WORK}/squashfs-root"
PREFIX="${APP}/usr/conda"

"${APP}/AppRun" --help > /dev/null
echo "AppRun runs from an extracted copy"

# Through usr/bin too, the symlink farm linuxdeploy is given rather than the
# scripts themselves, so an entry point that resolves its interpreter from its
# own path has to follow the link first
"${APP}/usr/bin/selkies" --help > /dev/null
"${PREFIX}/bin/pip" --version > /dev/null
"${PREFIX}/bin/conda" --version > /dev/null
echo "the entry points run, by either name"

# Not only the ones this project calls: a launcher left naming the build path
# is the whole class of defect, and a script nobody here runs is a user's first
# command as easily as any other. An interpreter under this copy passes, and so
# does one of the system's, which the shell prologue and the scripts conda
# ships for other languages name; anything else is a prefix that existed only
# on the machine that built the AppImage, and would resolve there.
outside="$(for path in "${PREFIX}"/bin/* "${PREFIX}"/condabin/*; do
    [ -f "${path}" ] || continue
    [ "$(head -c 2 "${path}" 2>/dev/null)" = '#!' ] || continue
    named="$(head -2 "${path}" | sed -n \
        -e 's|^#!\(/[^ ]*\).*|\1|p' \
        -e "s|^'''exec' \"\{0,1\}\(/[^\" ]*\).*|\1|p")"
    for interp in ${named}; do
        case "${interp}" in
            "${APP}"/*|/bin/*|/usr/bin/*|/usr/local/bin/*) ;;
            *) echo "${path##*/}: ${interp}" ;;
        esac
    done
done)"
if [ -n "${outside}" ]; then
    echo "::error::entry points naming an interpreter from neither this copy nor the system:"
    echo "${outside}"
    exit 1
fi
echo "every entry point names an interpreter inside this copy"

"${PREFIX}/bin/python" -c "import selkies, pixelflux, pcmflux"
echo "selkies, pixelflux and pcmflux import"

# The sound server AppRun starts, started by AppRun: a daemon that finds no
# module directory exits with "startup without any loaded modules", taking
# audio out of the AppImage while everything else still streams. The Wayland
# backend is selected so no X server is started, and the session AppRun goes on
# to attempt is beside the point -- what is checked is the daemon it leaves
# listening. Its own runtime directory, never a live session's.
RUNTIME="${WORK}/runtime"
mkdir -p "${RUNTIME}"
chmod 700 "${RUNTIME}"
# AppRun names this log, so an earlier run's copy would answer for this one
PULSE_LOG="/tmp/pulseaudio_selkies.log"
rm -f "${PULSE_LOG}"
env XDG_RUNTIME_DIR="${RUNTIME}" PULSE_SERVER="unix:${RUNTIME}/pulse/native" \
    PULSE_RUNTIME_PATH="${RUNTIME}/pulse" SELKIES_WAYLAND="true" \
    setsid "${APP}/AppRun" > "${WORK}/apprun.log" 2>&1 &
session="$!"
sinks=""
i=0
while [ "${i}" -lt 40 ]; do
    sinks="$(env XDG_RUNTIME_DIR="${RUNTIME}" PULSE_SERVER="unix:${RUNTIME}/pulse/native" \
        "${PREFIX}/bin/pactl" list short sinks 2>/dev/null || true)"
    [ -n "${sinks}" ] && break
    i=$((i + 1))
    sleep 1
done
kill -- "-${session}" 2>/dev/null || kill "${session}" 2>/dev/null || true
wait "${session}" 2>/dev/null || true
# The daemon outlives what AppRun exec'd and unlinks its socket on the way out
i=0
while [ -S "${RUNTIME}/pulse/native" ] && [ "${i}" -lt 10 ]; do
    i=$((i + 1))
    sleep 1
done
if [ -z "${sinks}" ]; then
    echo "::error::the sound server AppRun starts never came up; its log:"
    tail -n 20 "${PULSE_LOG}" 2>/dev/null || true
    exit 1
fi
# Where it loaded them from, and not only that it loaded some: the directory it
# has compiled in is the build path, which resolves on the build machine
modules="$(sed -n 's|.*Using modules directory \(.*\)\.$|\1|p' "${PULSE_LOG}" | tail -1)"
case "${modules}" in
    "${APP}"/*) ;;
    *)
        echo "::error::AppRun's sound server took its modules from ${modules:-nowhere it named}"
        exit 1
        ;;
esac
echo "AppRun's sound server comes up with a sink, on this copy's modules"

# The mount a user's own run takes: the runtime mounts the image read-only and
# only extracts itself where FUSE is missing, so the checks above never see it.
if [ -e /dev/fuse ]; then
    "${IMAGE}" --appimage-mount > "${WORK}/mount" 2>&1 &
    mounted="$!"
    point=""
    i=0
    while [ "${i}" -lt 20 ]; do
        point="$(head -1 "${WORK}/mount" 2>/dev/null || true)"
        [ -n "${point}" ] && [ -x "${point}/AppRun" ] && break
        i=$((i + 1))
        sleep 1
    done
    if [ -z "${point}" ] || [ ! -x "${point}/AppRun" ]; then
        echo "::error::the image did not mount; the runtime said:"
        cat "${WORK}/mount" 2>/dev/null || true
        kill "${mounted}" 2>/dev/null || true
        exit 1
    fi
    "${point}/AppRun" --help > /dev/null
    "${point}/usr/conda/bin/selkies" --help > /dev/null
    kill "${mounted}" 2>/dev/null || true
    wait "${mounted}" 2>/dev/null || true
    echo "AppRun and the entry points run from the read-only mount too"
else
    echo "note: no /dev/fuse here, so the read-only mount was not exercised"
fi
