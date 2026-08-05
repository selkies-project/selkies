#!/bin/bash

# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

set -e

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp/runtime-ubuntu}"

# Load the shared session environment computed by container-entrypoint.sh
# (interposer LD_PRELOAD, display, audio, and TURN defaults)
[ -f "${XDG_RUNTIME_DIR}/container-env" ] && . "${XDG_RUNTIME_DIR}/container-env"

# Wait for the X11 socket in the X11 backend; the Wayland backend owns its own
# headless compositor and needs no display server to wait for.
if [ "${SELKIES_WAYLAND:-false}" != "true" ]; then
  export DISPLAY="${DISPLAY:-:20}"
  echo 'Waiting for X Socket'
  until [ -S "/tmp/.X11-unix/X${DISPLAY#*:}" ]; do sleep 0.5; done
  echo 'X Server is ready'
  # Preset the resolution (additionally set SELKIES_ENABLE_RESIZE=true to fit the
  # remote resolution dynamically to the client window)
  selkies-resize 1920x1080
fi

exec selkies --addr=0.0.0.0 --port="${SELKIES_PORT:-8081}" "$@"
