#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Bring up the display and audio the integration and e2e tiers stream from: an
# Xvfb on E2E_DISPLAY (:99 unless set) with the canvas and extension set of the
# container and AppImage servers, and a PulseAudio with the null sink the audio
# suites capture. Each is left alone when it is already up, so this can run
# ahead of every suite, and nothing here reads DISPLAY: the suites inject input
# and resize the root window, so the display they get is never a desktop's.
#
#   E2E_DISPLAY=:99 scripts/ci/test-stack.sh
set -e
E2E_DISPLAY="${E2E_DISPLAY:-:99}"
export E2E_DISPLAY
socket="/tmp/.X11-unix/X${E2E_DISPLAY#*:}"
if [ ! -S "${socket}" ]; then
  # xrandr cannot grow a screen past its configured maximum, so a small canvas
  # makes every second-display mode fail to even be created
  Xvfb "${E2E_DISPLAY}" -screen 0 8192x4096x24 -s 0 -dpms \
    +extension "COMPOSITE" +extension "DAMAGE" +extension "GLX" \
    +extension "RANDR" +extension "RENDER" +extension "MIT-SHM" \
    +extension "XFIXES" +extension "XTEST" +iglx +render \
    -nolisten "tcp" -ac -noreset -shmem > "${TMPDIR:-/tmp}/Xvfb${E2E_DISPLAY#*:}.log" 2>&1 &
  until [ -S "${socket}" ]; do sleep 0.5; done
fi
if ! pactl info > /dev/null 2>&1; then
  pulseaudio --start --exit-idle-time=-1
fi
if ! pactl list short sinks | grep -q "[[:space:]]output[[:space:]]"; then
  pactl load-module module-null-sink sink_name=output rate=48000 channels=2 > /dev/null
fi
echo "E2E_DISPLAY=${E2E_DISPLAY}"
