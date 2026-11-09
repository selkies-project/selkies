#!/bin/bash

# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

set -e

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp/runtime-ubuntu}"

# Load the shared session environment computed by container-entrypoint.sh
# (interposer LD_PRELOAD, display, audio, and TURN defaults)
# shellcheck disable=SC1091  # written by container-entrypoint.sh at startup
[ -f "${XDG_RUNTIME_DIR}/container-env" ] && . "${XDG_RUNTIME_DIR}/container-env"

# The interposers belong to the session's applications, never to the backend
# that serves them: they answer for /dev/video0 and /dev/input, which capture
# and gamepads must keep seeing as the kernel reports them, and their
# process-wide read/close/ioctl/epoll_ctl hooks would block the server's event
# loop. SELKIES_INTERPOSER stays exported, telling the backend applications
# reach gamepads through the preload. Dropped by value, so an operator-supplied
# LD_PRELOAD survives.
if [ -n "${LD_PRELOAD:-}" ]; then
  kept=""
  rest="${LD_PRELOAD}"
  while [ -n "${rest}" ]; do
    entry="${rest%%:*}"
    case "${rest}" in *:*) rest="${rest#*:}" ;; *) rest="" ;; esac
    case "${entry}" in
      "${SELKIES_INTERPOSER:-$}"|"${SELKIES_WEBCAM_INTERPOSER:-$}"|"${FAKE_UDEV_LIB:-$}") continue ;;
    esac
    kept="${kept:+${kept}:}${entry}"
  done
  export LD_PRELOAD="${kept}"
fi

# Backend toggle, resolved as the server's settings resolve it: SELKIES_WAYLAND when set
# (blank included), else the legacy PIXELFLUX_WAYLAND, with "true" or "1" in any
# case ahead of a "|locked" suffix. Repeated here for running this script alone;
# container-entrypoint.sh has already canonicalized it for the service.
wayland="${SELKIES_WAYLAND-${PIXELFLUX_WAYLAND-}}"
wayland="${wayland%%|*}"
wayland="${wayland#"${wayland%%[![:space:]]*}"}"
wayland="${wayland%"${wayland##*[![:space:]]}"}"
wayland="${wayland,,}"

# Dev mode: run the server built in a checkout mounted into the container instead
# of the one baked into the image, so a change can be tried without rebuilding
# the image. The checkout's own web client (dist/web) is served from disk where
# it has one, so a rebuilt client takes effect on a reload; else the server
# serves the client compiled into it.
selkies="selkies"
if [ -n "${SELKIES_DEV_SOURCE:-}" ]; then
  built="${SELKIES_DEV_SOURCE}/target/release/selkies"
  if [ -x "${built}" ]; then
    selkies="${built}"
    if [ -f "${SELKIES_DEV_SOURCE}/dist/web/index.html" ]; then
      export SELKIES_WEB_BUNDLE="${SELKIES_DEV_SOURCE}/dist/web"
    fi
    echo "Dev mode: serving ${built}"
  else
    echo "Dev mode: ${SELKIES_DEV_SOURCE} holds no target/release/selkies; serving the image's own server" >&2
  fi
fi

# Wait for the X11 socket in the X11 backend; the Wayland backend owns its own
# headless compositor and needs no display server to wait for.
if [ "${wayland}" != "true" ] && [ "${wayland}" != "1" ]; then
  export DISPLAY="${DISPLAY:-:20}"
  echo 'Waiting for X Socket'
  until [ -S "/tmp/.X11-unix/X${DISPLAY#*:}" ]; do sleep 0.5; done
  echo 'X Server is ready'
  # Preset the resolution, which dynamic resizing (SELKIES_ENABLE_RESIZE, on by
  # default) replaces with the client window's as soon as a client connects: the
  # size a manual resolution locks the stream to, as the server resolves it from
  # what it is started with below (`selkies manual-size`), else DISPLAY_SIZEW by
  # DISPLAY_SIZEH. A display already that size keeps the mode its X server came
  # up in.
  size="$("${selkies}" manual-size "$@" 2>/dev/null)" || size=""
  size="${size:-${DISPLAY_SIZEW:-1920}x${DISPLAY_SIZEH:-1080}}"
  if [ "$(xdpyinfo 2>/dev/null | awk '/dimensions:/ {print $2; exit}')" != "${size}" ]; then
    "${selkies}" resize "${size}"
  fi
fi

# The published port is the container's boundary, so listen on every interface
exec "${selkies}" --public --port="${SELKIES_PORT:-8080}" "$@"
