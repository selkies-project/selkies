#!/bin/bash

# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

set -e

# Wait for XDG_RUNTIME_DIR
until [ -d "${XDG_RUNTIME_DIR}" ]; do sleep 0.5; done

# Configure joystick interposer
export LIB_PREFIX="/usr/\$LIB"
export SELKIES_INTERPOSER="${LIB_PREFIX}/selkies_joystick_interposer.so"
export LIBUDEV_PACKAGE="${LIBUDEV_PACKAGE:-libudev}"
export LIBUDEV_PKG_VERSION="${LIBUDEV_PKG_VERSION:-0.0.0}"
export FAKE_UDEV_LIB="${LIB_PREFIX}/${LIBUDEV_PACKAGE}.so.${LIBUDEV_PKG_VERSION}-fake"
export LD_PRELOAD="${SELKIES_INTERPOSER}:${FAKE_UDEV_LIB}${LD_PRELOAD:+:${LD_PRELOAD}}"
export SDL_JOYSTICK_DEVICE=/dev/input/js0
mkdir -pm1777 /dev/input || sudo-root mkdir -pm1777 /dev/input || echo 'Failed to create joystick interposer directory'

if [ -d /dev/input ]; then
  mknod /dev/input/js0 c 13 0 || sudo-root mknod /dev/input/js0 c 13 0 || echo "Failed to create joystick device file 0"
  mknod /dev/input/js1 c 13 1 || sudo-root mknod /dev/input/js1 c 13 1 || echo "Failed to create joystick device file 1"
  mknod /dev/input/js2 c 13 2 || sudo-root mknod /dev/input/js2 c 13 2 || echo "Failed to create joystick device file 2"
  mknod /dev/input/js3 c 13 3 || sudo-root mknod /dev/input/js3 c 13 3 || echo "Failed to create joystick device file 3"
  mknod /dev/input/event1000 c 13 1064 || sudo-root mknod /dev/input/event1000 c 13 1064 || echo "Failed to create event device file 1000"
  mknod /dev/input/event1001 c 13 1065 || sudo-root mknod /dev/input/event1001 c 13 1065 || echo "Failed to create event device file 1001"
  mknod /dev/input/event1002 c 13 1066 || sudo-root mknod /dev/input/event1002 c 13 1066 || echo "Failed to create event device file 1002"
  mknod /dev/input/event1003 c 13 1067 || sudo-root mknod /dev/input/event1003 c 13 1067 || echo "Failed to create event device file 1003"
  chmod 0666 /dev/input/js* /dev/input/event* || sudo-root chmod 0666 /dev/input/js* /dev/input/event* || echo "Failed to change permission for joystick interposer devices"
else
  echo "Skipping Joystick interposer device files creation since /dev/input is unavailable"
fi

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

# Opt-in: disable screen blanking, DPMS, and screen lockers so a locked/blanked
# session does not stop framebuffer capture (default off, current behavior unchanged).
# See https://github.com/selkies-project/selkies/issues/174
if [ "${SELKIES_DISABLE_SCREEN_BLANKING:-false}" = "1" ] || [ "$(echo "${SELKIES_DISABLE_SCREEN_BLANKING:-false}" | tr '[:upper:]' '[:lower:]')" = "true" ]; then
  # Turn off the built-in X screen saver, screen blanking, and DPMS power management
  xset s off || true
  xset s noblank || true
  xset -dpms || true
  # Prevent common desktop screen lockers from autostarting and blanking the framebuffer on lock
  mkdir -p "${HOME}/.config/autostart"
  for locker in light-locker xscreensaver xfce4-screensaver gnome-screensaver mate-screensaver xss-lock; do
    printf '[Desktop Entry]\nType=Application\nName=%s\nHidden=true\nX-GNOME-Autostart-enabled=false\n' "${locker}" > "${HOME}/.config/autostart/${locker}.desktop"
  done
fi

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