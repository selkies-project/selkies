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

# The interposers belong to the session's applications, never to the backend that
# serves them: they answer for /dev/video0 and /dev/input, which capture and
# gamepads must keep seeing as the kernel reports them, and their process-wide
# read/close/ioctl/epoll_ctl hooks block the asyncio loop. SELKIES_INTERPOSER
# stays exported, telling the backend applications reach gamepads through the
# preload. Dropped by value, so an operator-supplied LD_PRELOAD survives.
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

# Backend toggle, resolved as settings.py resolves it: SELKIES_WAYLAND when set
# (blank included), else the legacy PIXELFLUX_WAYLAND, with "true" or "1" in any
# case ahead of a "|locked" suffix. Repeated here for running this script alone;
# container-entrypoint.sh has already canonicalized it for the service.
wayland="${SELKIES_WAYLAND-${PIXELFLUX_WAYLAND-}}"
wayland="${wayland%%|*}"
wayland="${wayland#"${wayland%%[![:space:]]*}"}"
wayland="${wayland%"${wayland##*[![:space:]]}"}"
wayland="${wayland,,}"

# Dev mode: run a checkout mounted into the container instead of the package
# baked into the image, so a change can be tried without rebuilding. The web
# client is a build product and is absent from a fresh checkout, so the image's
# own bundle is linked in where the tree has none -- the path is gitignored, so
# the link does not show up as a change in the checkout.
if [ -n "${SELKIES_DEV_SOURCE:-}" ]; then
  if [ -d "${SELKIES_DEV_SOURCE}/src/selkies" ]; then
    bundled="$(python3 -c 'import os, selkies; print(os.path.join(os.path.dirname(selkies.__file__), "selkies_web"))' 2>/dev/null || true)"
    if [ ! -e "${SELKIES_DEV_SOURCE}/src/selkies/selkies_web" ] && [ -d "${bundled}" ]; then
      ln -sfn "${bundled}" "${SELKIES_DEV_SOURCE}/src/selkies/selkies_web" 2>/dev/null ||
        echo "Dev mode: no web client in the checkout and its src/selkies is not writable" >&2
    fi
    export PYTHONPATH="${SELKIES_DEV_SOURCE}/src${PYTHONPATH:+:${PYTHONPATH}}"
    echo "Dev mode: serving from ${SELKIES_DEV_SOURCE}/src"
  else
    echo "Dev mode: ${SELKIES_DEV_SOURCE} holds no src/selkies; serving the image's own package" >&2
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
  # default) replaces with the client window's as soon as a client connects
  selkies-resize 1920x1080
fi

# The published port is the container's boundary, so listen on every interface
exec selkies --addr=0.0.0.0 --port="${SELKIES_PORT:-8080}" "$@"
