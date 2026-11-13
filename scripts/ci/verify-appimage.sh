#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Prove the AppImage runs from an extracted copy on a host that has none of
# what it carries: run as root on a pristine image at the bundle's glibc floor,
# with curl the one tool it needs.
#
# What AppRun starts by itself is checked first: on the Wayland backend it
# starts no X server, only the bundle's PulseAudio, and the server's compositor
# comes up and serves the web client it embeds. Then `session`, run as an
# ordinary user, brings up the bundle's own X server, buses, and sound server.
# Where /dev/fuse is present the read-only mount a user's own run takes is
# exercised too.
#
# Usage: verify-appimage.sh <appimage>
set -eu

IMAGE="$(readlink -f "${1:?usage: verify-appimage.sh <appimage>}")"
WORK="$(mktemp -d)"
chmod 755 "${WORK}"
trap 'rm -rf "${WORK}" 2>/dev/null || true' EXIT
cd "${WORK}"
"${IMAGE}" --appimage-extract > /dev/null
APP="${WORK}/squashfs-root"
chmod -R a+rX "${APP}"

"${APP}/AppRun" --version | grep -F "web client: embedded ("
echo "AppRun runs the server from an extracted copy, with its web client"

missing="$(find "${APP}/bin" "${APP}/libexec" -type f | while read -r f; do
    head -c4 "${f}" | grep -q ELF || continue
    ldd "${f}" 2>/dev/null | grep 'not found' | sed "s|^|${f#"${APP}"/}: |"
done | sort -u)"
if [ -n "${missing}" ]; then
    echo "::error::programs with unresolved dependencies:"
    echo "${missing}"
    exit 1
fi
echo "every program the AppImage carries resolves what it links"

PORT=18080
# The entry page's first content-hashed script, then its head fetched taking gzip
serves_client() {
    asset=""
    i=0
    while [ -z "${asset}" ] && [ "${i}" -lt "$1" ]; do
        asset="$(curl -fsS "http://127.0.0.1:${PORT}/" 2>/dev/null | grep -o 'assets/[^"]*\.js' | head -n 1 || true)"
        [ -n "${asset}" ] || sleep 1
        i=$((i + 1))
    done
    [ -n "${asset}" ] || return 1
    curl -fsS -D - -o /dev/null -H "Accept-Encoding: gzip" "http://127.0.0.1:${PORT}/${asset}" \
        | tr -d '\r' | grep -qi '^content-encoding: gzip$'
}

# AppRun alone: the bundle's PulseAudio where none listens, and the server
RUNTIME="${WORK}/runtime"
mkdir -p "${RUNTIME}"
chmod 700 "${RUNTIME}"
env XDG_RUNTIME_DIR="${RUNTIME}" SELKIES_WAYLAND=true SELKIES_PORT="${PORT}" \
    setsid "${APP}/AppRun" --enable-basic-auth=false > "${WORK}/apprun.log" 2>&1 &
apprun="$!"
if ! serves_client 60; then
    echo "::error::the server AppRun starts never served its web client; its log:"
    tail -n 20 "${WORK}/apprun.log"
    exit 1
fi
if [ ! -S "${RUNTIME}/pulse/native" ]; then
    echo "::error::the sound server AppRun starts never came up; its log:"
    tail -n 20 "${RUNTIME}/pulseaudio_selkies.log" 2>/dev/null || true
    exit 1
fi
grep -q "Wayland compositor socket: " "${WORK}/apprun.log" || {
    echo "::error::the Wayland compositor never came up; its log:"
    tail -n 20 "${WORK}/apprun.log"
    exit 1
}
kill -- "-${apprun}" 2>/dev/null || kill "${apprun}" 2>/dev/null || true
wait "${apprun}" 2>/dev/null || true
echo "AppRun starts the bundle's sound server, and the server's compositor serves its web client gzip-encoded"

# A session as an ordinary user
useradd -m selkies-check 2> /dev/null || adduser -D selkies-check
mkdir -p /tmp/.X11-unix && chmod 1777 /tmp/.X11-unix
su selkies-check -c "exec setsid '${APP}/AppRun' session --runtime-dir /tmp/selkies-check \
    --session 'sleep 600' -- --port=${PORT} --enable-basic-auth=false" > "${WORK}/session.log" 2>&1 &
session="$!"
if ! serves_client 60; then
    echo "::error::the session never served its web client; its log:"
    tail -n 30 "${WORK}/session.log"
    exit 1
fi
running="$(for proc in /proc/[0-9]*; do
    tr '\0' '\n' < "${proc}/cmdline" 2> /dev/null | head -n 1 || true
done)"
for program in Xvfb dbus-daemon pulseaudio; do
    printf '%s\n' "${running}" | grep -qx "${APP}/libexec/selkies/${program}" || {
        echo "::error::the session runs no bundled ${program}; its log:"
        tail -n 30 "${WORK}/session.log"
        exit 1
    }
done
kill -- "-${session}" 2>/dev/null || kill "${session}" 2>/dev/null || true
echo "AppRun session runs the bundle's X server, buses, and sound server"

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
    kill "${mounted}" 2>/dev/null || true
    wait "${mounted}" 2>/dev/null || true
    echo "AppRun runs from the read-only mount too"
else
    echo "note: no /dev/fuse here, so the read-only mount was not exercised"
fi
