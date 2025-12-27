#!/bin/bash

# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

set -e

# Wait for XDG_RUNTIME_DIR
until [ -d "${XDG_RUNTIME_DIR}" ]; do sleep 0.5; done

# Configure joystick interposer
export SELKIES_INTERPOSER='/usr/$LIB/selkies_joystick_interposer.so'
export LD_PRELOAD="${SELKIES_INTERPOSER}${LD_PRELOAD:+:${LD_PRELOAD}}"
export SDL_JOYSTICK_DEVICE=/dev/input/js0
mkdir -pm1777 /dev/input || sudo-root mkdir -pm1777 /dev/input || echo 'Failed to create joystick interposer directory'
touch /dev/input/js0 /dev/input/js1 /dev/input/js2 /dev/input/js3 || sudo-root touch /dev/input/js0 /dev/input/js1 /dev/input/js2 /dev/input/js3 || echo 'Failed to create joystick interposer devices'
chmod 777 /dev/input/js* || sudo-root chmod 777 /dev/input/js* || echo 'Failed to change permission for joystick interposer devices'

# Set default display
export DISPLAY="${DISPLAY:-:20}"
# PipeWire-Pulse server socket path
export PIPEWIRE_LATENCY="128/48000"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp}"
export PIPEWIRE_RUNTIME_DIR="${PIPEWIRE_RUNTIME_DIR:-${XDG_RUNTIME_DIR:-/tmp}}"
export PULSE_RUNTIME_PATH="${PULSE_RUNTIME_PATH:-${XDG_RUNTIME_DIR:-/tmp}/pulse}"
export PULSE_SERVER="${PULSE_SERVER:-unix:${PULSE_RUNTIME_PATH:-${XDG_RUNTIME_DIR:-/tmp}/pulse}/native}"

# Start X server with required extensions
/usr/bin/Xvfb "${DISPLAY}" -screen 0 "8192x4096x24" +extension "COMPOSITE" +extension "DAMAGE" +extension "GLX" +extension "RANDR" +extension "RENDER" +extension "MIT-SHM" +extension "XFIXES" +extension "XTEST" +iglx +render -nolisten "tcp" -ac -noreset -shmem >/tmp/Xvfb.log 2>&1 &

# Wait for X server to start
echo 'Waiting for X Socket' && until [ -S "/tmp/.X11-unix/X${DISPLAY#*:}" ]; do sleep 0.5; done && echo 'X Server is ready'

# Preset the resolution
selkies-resize 1920x1080

# Start Xfce4 Desktop session
[ "${START_XFCE4:-true}" = "true" ] && rm -rf ~/.config/xfce4 && vglrun -d "${VGL_DISPLAY:-egl}" /usr/bin/dbus-launch --exit-with-session /usr/bin/xfce4-session &

# Add proot-apps
if [ ! -f "${HOME}/.local/bin/proot-apps" ]; then
  mkdir -p ${HOME}/.local/bin/
  cp /tmp/proot-apps/* ${HOME}/.local/bin/
  echo 'export PATH="$HOME/.local/bin:$PATH"' >> $HOME/.bashrc
  chown ${USER}:${USER} \
    ${HOME}/.bashrc \
    ${HOME}/.local/ \
    ${HOME}/.local/bin \
    ${HOME}/.local/bin/{ncat,proot-apps,proot,jq,pversion}
elif ! diff -q /tmp/proot-apps/pversion ${HOME}/.local/bin/pversion > /dev/null; then
  cp /tmp/proot-apps/* ${HOME}/.local/bin/
  chown ${USER}:${USER} ${HOME}/.local/bin/{ncat,proot-apps,proot,jq,pversion}
fi

echo "Session Running. Press [Return] to exit."
read