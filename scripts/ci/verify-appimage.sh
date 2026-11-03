#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Prove the AppImage in $1 works somewhere other than where it was built.
#
# What ships mounts at a path chosen at run time, so every path the build
# could have left pointing at the build machine is checked from an extracted
# copy, which is also how a user reaches the payload by hand: the libraries
# the server and the bundled sound server link resolve inside the copy or on
# the host, the server finds its web client beside itself, and the sound server
# AppRun starts takes its modules from the copy rather than from the module
# directory its build compiled in. Run on a pristine host at the glibc floor,
# whose own libraries cannot stand in for one the AppImage forgot.
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

"${IMAGE}" --appimage-extract-and-run --version
"${APP}/AppRun" --version
"${APP}/AppRun" --help > /dev/null
echo "AppRun runs from an extracted copy"
"${APP}/AppRun" session --help > /dev/null
"${APP}/usr/libexec/selkies-session/pulseaudio" --version > /dev/null
echo "AppRun runs the session launcher, and its sound server wrapper finds the daemon"
"${APP}/usr/bin/selkies" --help > /dev/null
echo "the server runs by its own path"
test -f "${APP}/usr/share/selkies/web/index.html"
echo "the web client sits beside the server"

# Every executable and library, the PulseAudio modules among them, resolves
# through what the copy carries and what the host provides
missing="$(find "${APP}" -type f \( -perm -u+x -o -name '*.so*' \) | while read -r f; do
    [ "$(head -c 4 "${f}" | od -An -c | tr -d ' ')" = '177ELF' ] || continue
    ldd "${f}" 2>/dev/null | grep 'not found' | sed "s|^|${f#"${APP}"/}: |"
done | sort -u)"
if [ -n "${missing}" ]; then
    echo "::error::bundled files with unresolved libraries:"
    echo "${missing}"
    exit 1
fi
echo "every bundled ELF file resolves"
for lib in selkies_input_interposer.so selkies_v4l2_interposer.so; do
    test -f "${APP}/usr/lib/${lib}"
done
echo "the interposers are where AppRun exports them"

# The sound server AppRun starts, started by AppRun: a daemon that finds no
# module directory exits with "startup without any loaded modules", taking
# audio out of the AppImage while everything else still streams. The Wayland
# backend is selected so no X server is started, and its compositor is checked
# too. Its own runtime directory, never a live session's, and no login, which
# the server otherwise refuses to start without a password for.
RUNTIME="${WORK}/runtime"
mkdir -p "${RUNTIME}"
chmod 700 "${RUNTIME}"
# AppRun names this log, so an earlier run's copy would answer for this one
PULSE_LOG="/tmp/pulseaudio_selkies.log"
rm -f "${PULSE_LOG}"
env XDG_RUNTIME_DIR="${RUNTIME}" PULSE_SERVER="unix:${RUNTIME}/pulse/native" \
    PULSE_RUNTIME_PATH="${RUNTIME}/pulse" SELKIES_WAYLAND="true" \
    setsid "${APP}/AppRun" --enable-basic-auth=false > "${WORK}/apprun.log" 2>&1 &
session="$!"
sinks=""
i=0
while [ "${i}" -lt 40 ]; do
    sinks="$(env XDG_RUNTIME_DIR="${RUNTIME}" PULSE_SERVER="unix:${RUNTIME}/pulse/native" \
        "${APP}/usr/bin/pactl" list short sinks 2>/dev/null || true)"
    [ -n "${sinks}" ] && break
    i=$((i + 1))
    sleep 1
done
# The compositor names its socket once up, or says its startup window passed
i=0
while ! grep -q "Wayland compositor socket" "${WORK}/apprun.log" && [ "${i}" -lt 30 ]; do
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
if ! grep -q "Wayland compositor socket: " "${WORK}/apprun.log"; then
    echo "::error::the Wayland compositor AppRun starts never came up; its log:"
    tail -n 20 "${WORK}/apprun.log"
    exit 1
fi
echo "AppRun's Wayland compositor comes up"

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
    "${point}/AppRun" session --help > /dev/null
    "${point}/usr/bin/selkies" --help > /dev/null
    kill "${mounted}" 2>/dev/null || true
    wait "${mounted}" 2>/dev/null || true
    echo "AppRun and the server run from the read-only mount too"
else
    echo "note: no /dev/fuse here, so the read-only mount was not exercised"
fi
