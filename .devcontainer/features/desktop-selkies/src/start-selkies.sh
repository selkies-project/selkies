#!/bin/bash

# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

set -e

# The X display is the session only on the X11 backend; on Wayland a DISPLAY
# in the server's environment would be taken for an Xwayland hosting the
# desktop, so none is exported there.
if [ "${SELKIES_WAYLAND:-false}" != "true" ]; then
    export DISPLAY="${DISPLAY:-:20}"
    unset WAYLAND_DISPLAY
else
    unset DISPLAY
fi
export XSERVER="${XSERVER:-XVFB}"

SCRIPT_DIR="$(dirname "$(readlink -f "$0")")"

function cleanup() {
    pkill -9 -x turnserver 1>/dev/null 2>&1 || true
    pgrep -af '.*selkies.*' | cut -d' ' -f1 | xargs kill -9 1>/dev/null 2>&1 || true
    pgrep -afi '.*lxqt.*' | cut -d' ' -f1 | xargs kill -9 1>/dev/null 2>&1 || true
    sudo /usr/bin/pulseaudio -k 1>/dev/null 2>&1 || true
    pkill -9 -x Xvfb 1>/dev/null 2>&1 || true
    exit
}
# SIGKILL cannot be trapped; SIGTERM is what a stopping container sends.
trap cleanup SIGINT SIGTERM EXIT

if [ "${SELKIES_WAYLAND:-false}" != "true" ]; then
    # Start Xvfb Xserver
    if [ "${XSERVER}" = "XVFB" ]; then
        Xvfb "${DISPLAY}" -screen 0 8192x4096x24 -s 0 -dpms +extension "COMPOSITE" +extension "DAMAGE" +extension "GLX" +extension "RANDR" +extension "RENDER" +extension "MIT-SHM" +extension "XFIXES" +extension "XTEST" +iglx +render -nolisten "tcp" -ac -noreset -shmem >/tmp/Xvfb.log 2>&1 &
    fi
    # Wait for X server to start
    echo 'Waiting for X Socket' && until [ -S "/tmp/.X11-unix/X${DISPLAY#*:}" ]; do sleep 0.5; done && echo 'X Server is ready'
    # Disable screen saver/blanking/power management
    xset s off && xset s noblank && xset -dpms
fi

# Start PulseAudio server
export PULSE_SERVER="tcp:127.0.0.1:4713"
sudo /usr/bin/pulseaudio -k >/dev/null 2>&1 || true
sudo /usr/bin/pulseaudio --system --verbose --log-target=file:/tmp/pulseaudio.log --realtime=true --disallow-exit -L 'module-native-protocol-tcp auth-ip-acl=127.0.0.0/8 port=4713 auth-anonymous=1' &

# The interposer augments this directory with its evdev nodes and intercepts the
# joydev/evdev paths by name, so only the directory itself has to exist.
sudo mkdir -pm1777 /dev/input

# If installed, add the joystick interposer to LD_PRELOAD
if [ -e "/usr/lib/x86_64-linux-gnu/selkies_joystick_interposer.so" ]; then
    # $LIB is a dynamic-loader token; the backslash keeps the shell off it.
    export SELKIES_INTERPOSER="/usr/\$LIB/selkies_joystick_interposer.so"
    export LD_PRELOAD="${SELKIES_INTERPOSER}${LD_PRELOAD:+:${LD_PRELOAD}}"
    # No fake-udev here, so SDL2 applications need the evdev nodes by name. One
    # entry per slot: naming js0 as well would show slot 0 twice.
    export SDL_JOYSTICK_DEVICE="/dev/input/event1000:/dev/input/event1001:/dev/input/event1002:/dev/input/event1003"
fi

# Start the LXQt desktop session (X11 backend only)
if [ "${SELKIES_WAYLAND:-false}" != "true" ]; then
    case ${DESKTOP:-LXQT} in
        LXQT)
            lxqt-session &
            ;;
        NONE)
            ;;
        *)
            echo "WARN: Unsupported DESKTOP: '${DESKTOP}'"
            ;;
    esac
fi

# Start turnserver
"${SCRIPT_DIR}/start-turnserver.sh" &

# Preset the resolution (X11 backend only)
[ "${SELKIES_WAYLAND:-false}" != "true" ] && selkies-resize 1920x1080

# Start Selkies
exec selkies \
    --addr="0.0.0.0,::" \
    --port="${SELKIES_PORT:-${WEB_PORT:-8080}}" \
    --enable-resize="${SELKIES_ENABLE_RESIZE:-true}" \
    "$@"
