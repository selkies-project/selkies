# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

export DISPLAY=:20
unset WAYLAND_DISPLAY
export XSERVER=${XSERVER:-XVFB}

SCRIPT_DIR=$(dirname $(readlink -f $0))

function cleanup() {
    kill -9 $(pidof turnserver) 1>/dev/null 2>&1|| true
    pgrep -af '.*selkies.*' | cut -d' ' -f1 | xargs kill -9 1>/dev/null 2>&1|| true
    pgrep -afi '.*xfce4.*' | cut -d' ' -f1 | xargs kill -9 1>/dev/null 2>&1|| true
    pgrep -afi '.*fluxbox.*' | cut -d' ' -f1 | xargs kill -9 1>/dev/null 2>&1|| true
    sudo /usr/bin/pulseaudio -k 1>/dev/null 2>&1  || true
    kill -9 $(pidof Xvfb) 1>/dev/null 2>&1 || true
    exit
}
trap cleanup SIGINT SIGKILL EXIT

# Start Xvfb Xserver
if [ "${XSERVER}" = "XVFB" ]; then
    Xvfb "${DISPLAY}" -screen 0 8192x4096x24 +extension "COMPOSITE" +extension "DAMAGE" +extension "GLX" +extension "RANDR" +extension "RENDER" +extension "MIT-SHM" +extension "XFIXES" +extension "XTEST" +iglx +render -nolisten "tcp" -ac -noreset -shmem >/tmp/Xvfb.log 2>&1 &
fi

# Wait for X server to start
echo 'Waiting for X Socket' && until [ -S "/tmp/.X11-unix/X${DISPLAY#*:}" ]; do sleep 0.5; done && echo 'X Server is ready'

# Disable screen saver
xset s off
# Disable screen blanking
xset s noblank
# Disable power management
xset -dpms

# Start PulseAudio server
export PULSE_SERVER=tcp:127.0.0.1:4713
sudo /usr/bin/pulseaudio -k >/dev/null 2>&1
sudo /usr/bin/pulseaudio --system --verbose --log-target=file:/tmp/pulseaudio.log --realtime=true --disallow-exit -L 'module-native-protocol-tcp auth-ip-acl=127.0.0.0/8 port=4713 auth-anonymous=1' &

# Create /dev/input/jsX if they don't already exists
sudo mkdir -pm1777 /dev/input
sudo touch /dev/input/{js0,js1,js2,js3}
sudo chmod 777 /dev/input/js*

# If installed, add the joystick interposer to LD_PRELOAD
if [ -e "/usr/lib/x86_64-linux-gnu/selkies_joystick_interposer.so" ]; then
    export SELKIES_INTERPOSER='/usr/$LIB/selkies_joystick_interposer.so'
    export LD_PRELOAD="${SELKIES_INTERPOSER}${LD_PRELOAD:+:${LD_PRELOAD}}"
    export SDL_JOYSTICK_DEVICE=/dev/input/js0
fi

# Start desktop environment
case ${DESKTOP:-XFCE} in
    FLUXBOX)
        startfluxbox &
        ;;
    XFCE)
        xfce4-session &
        ;;
    *)
        echo "WARN: Unsupported DESKTOP: '${DESKTOP}'"
        ;;
esac

# Source gstreamer environment
. /opt/gstreamer/gst-env

# Start turnserver
${SCRIPT_DIR}/start-turnserver.sh &

# Start Selkies
selkies-resize 1920x1080
selkies \
    --addr="0.0.0.0" \
    --port="${WEB_PORT:-6080}" \
    --metrics_port=${SELKIES_METRICS_PORT:-19090} \
    --cursor_size=${SELKIES_CURSOR_SIZE:-"-1"} \
    --enable_resize=${SELKIES_ENABLE_RESIZE:-true} \
    --turn_host=${SELKIES_TURN_HOST:-localhost} \
    --turn_port=${SELKIES_TURN_PORT:-3478} \
    --turn_username=${SELKIES_TURN_USERNAME:-selkies} \
    --turn_password=${SELKIES_TURN_PASSWORD:-selkies} \
    --turn_protocol=${SELKIES_TURN_PROTOCOL:-tcp} \
    $@
