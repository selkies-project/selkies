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
# that serves them: they answer for /dev/video0 and /dev/input, which the
# capture and gamepad sides have to keep seeing as the kernel reports them,
# and they hook read/close/ioctl/epoll_ctl process-wide -- one of those hooks
# blocking is the asyncio loop blocking, and a frozen loop answers nothing.
# SELKIES_INTERPOSER stays exported, which is what tells the backend gamepads
# reach applications through the preload. Dropped by value, so an
# operator-supplied LD_PRELOAD survives.
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

# Backend toggle, resolved the way settings.py resolves it: SELKIES_WAYLAND when
# set (blank included, which means the default), else the legacy PIXELFLUX_WAYLAND;
# "true" or "1", in any case and ahead of a "|locked" suffix, selects Wayland.
# container-entrypoint.sh has already canonicalized the variable for the service;
# the resolution here is for running this script on its own.
wayland="${SELKIES_WAYLAND-${PIXELFLUX_WAYLAND-}}"
wayland="${wayland%%|*}"
wayland="${wayland#"${wayland%%[![:space:]]*}"}"
wayland="${wayland%"${wayland##*[![:space:]]}"}"
wayland="${wayland,,}"

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

exec selkies --addr=0.0.0.0 --port="${SELKIES_PORT:-8080}" "$@"
