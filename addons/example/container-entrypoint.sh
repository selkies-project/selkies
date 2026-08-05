#!/bin/bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# PID-agnostic container init for the Selkies example container. It prepares the
# runtime environment (joystick interposer + fake-udev LD_PRELOAD, device nodes,
# TURN defaults), derives the service set from the environment toggles, and then
# hands service supervision to s6 (`s6-svscan /etc/service`): one `s6-supervise`
# per service directory, restarts crashed services, and is controlled with
# `s6-svc`/`s6-svstat` (the supervisorctl equivalent). s6-svscan does NOT need
# to be PID 1 (unlike s6-overlay): it can be launched below any injected init
# or script, and equally works when this script itself is PID 1 (in which case
# `docker run --init` for zombie reaping is recommended).

set -e

# Wait for XDG_RUNTIME_DIR to exist (created by the image ENV or the caller)
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp/runtime-ubuntu}"
mkdir -pm700 "${XDG_RUNTIME_DIR}"

# Configure joystick interposer and fake-udev (container-only gamepad plumbing)
export LIB_PREFIX="/usr/\$LIB"
export SELKIES_INTERPOSER="${LIB_PREFIX}/selkies_joystick_interposer.so"
export LIBUDEV_PACKAGE="${LIBUDEV_PACKAGE:-libudev}"
export LIBUDEV_PKG_VERSION="${LIBUDEV_PKG_VERSION:-1.0.0}"
export FAKE_UDEV_LIB="${LIB_PREFIX}/${LIBUDEV_PACKAGE}.so.${LIBUDEV_PKG_VERSION}-fake"
export LD_PRELOAD="${SELKIES_INTERPOSER}:${FAKE_UDEV_LIB}${LD_PRELOAD:+:${LD_PRELOAD}}"
export SDL_JOYSTICK_DEVICE=/dev/input/js0
mkdir -pm1777 /dev/input || sudo-root mkdir -pm1777 /dev/input || echo 'Failed to create joystick interposer device directory'

if [ -d /dev/input ]; then
  for i in 0 1 2 3; do
    mknod "/dev/input/js${i}" c 13 "${i}" || sudo-root mknod "/dev/input/js${i}" c 13 "${i}" || echo "Failed to create joystick device file ${i}"
    mknod "/dev/input/event100${i}" c 13 "106${i}" || sudo-root mknod "/dev/input/event100${i}" c 13 "106${i}" || echo "Failed to create event device file 100${i}"
  done
  chmod 0666 /dev/input/js* /dev/input/event* || sudo-root chmod 0666 /dev/input/js* /dev/input/event* || echo 'Failed to change permission for joystick interposer devices'
else
  echo 'Skipping joystick interposer device files creation since /dev/input is unavailable'
fi

# Default display for the X11 backend (unused in Wayland mode)
export DISPLAY="${DISPLAY:-:20}"
# PipeWire-Pulse server socket path
export PIPEWIRE_LATENCY="128/48000"
export PIPEWIRE_RUNTIME_DIR="${PIPEWIRE_RUNTIME_DIR:-${XDG_RUNTIME_DIR}}"
export PULSE_RUNTIME_PATH="${PULSE_RUNTIME_PATH:-${XDG_RUNTIME_DIR}/pulse}"
export PULSE_SERVER="${PULSE_SERVER:-unix:${PULSE_RUNTIME_PATH}/native}"

# Hardware OpenGL. On NVIDIA, Zink routes GL through the Vulkan driver; other
# vendors reach the GPU through the display server's render node instead (see
# services/xvfb/run). Both signals are required: the device nodes prove a GPU was
# passed in, and a working nvidia-smi proves the driver stack matches it. Set
# DISABLE_ZINK=true for llvmpipe.
if [ "${DISABLE_ZINK:-false}" != "true" ] && ls /dev/nvidia* >/dev/null 2>&1 && nvidia-smi >/dev/null 2>&1; then
  export LIBGL_KOPPER_DRI2=1
  export MESA_LOADER_DRIVER_OVERRIDE=zink
  export GALLIUM_DRIVER=zink
  echo 'NVIDIA GPU detected: OpenGL runs through Zink on the NVIDIA Vulkan driver'
fi

# Compute the shared session environment, including embedded coTURN defaults
ENV_FILE="${XDG_RUNTIME_DIR}/container-env"
: > "${ENV_FILE}"

export SELKIES_ENABLE_INTERNAL_TURN=false
if [ "${SELKIES_MODE:-websockets}" = "webrtc" ] || [ "${SELKIES_ENABLE_DUAL_MODE:-false}" = "true" ]; then
  if [ -z "${SELKIES_TURN_REST_URI}" ] && { { [ -z "${SELKIES_TURN_USERNAME}" ] || [ -z "${SELKIES_TURN_PASSWORD}" ]; } && [ -z "${SELKIES_TURN_SHARED_SECRET}" ] || [ -z "${SELKIES_TURN_HOST}" ] || [ -z "${SELKIES_TURN_PORT}" ]; }; then
    export SELKIES_ENABLE_INTERNAL_TURN=true
    export TURN_RANDOM_PASSWORD="$(tr -dc 'A-Za-z0-9' < /dev/urandom 2>/dev/null | head -c 24)"
    export SELKIES_TURN_HOST="${SELKIES_TURN_HOST:-$(dig -4 TXT +short @ns1.google.com o-o.myaddr.l.google.com 2>/dev/null | { read output; if [ -z "$output" ] || echo "$output" | grep -q '^;;'; then exit 1; else echo "$(echo $output | sed 's,",,g')"; fi } || dig -6 TXT +short @ns1.google.com o-o.myaddr.l.google.com 2>/dev/null | { read output; if [ -z "$output" ] || echo "$output" | grep -q '^;;'; then exit 1; else echo "[$(echo $output | sed 's,",,g')]"; fi } || hostname -I 2>/dev/null | awk '{print $1; exit}' || echo '127.0.0.1')}"
    export TURN_EXTERNAL_IP="${TURN_EXTERNAL_IP:-$(getent ahostsv4 $(echo ${SELKIES_TURN_HOST} | tr -d '[]') 2>/dev/null | awk '{print $1; exit}' || getent ahostsv6 $(echo ${SELKIES_TURN_HOST} | tr -d '[]') 2>/dev/null | awk '{print "[" $1 "]"; exit}')}"
    export SELKIES_TURN_PORT="${SELKIES_TURN_PORT:-3478}"
    export SELKIES_TURN_USERNAME="selkies"
    export SELKIES_TURN_PASSWORD="${TURN_RANDOM_PASSWORD}"
    export SELKIES_TURN_PROTOCOL="${SELKIES_TURN_PROTOCOL:-tcp}"
    export SELKIES_STUN_HOST="${SELKIES_STUN_HOST:-stun.l.google.com}"
    export SELKIES_STUN_PORT="${SELKIES_STUN_PORT:-19302}"
  fi
fi

# Persist the environment for the s6 services (they are siblings, not children,
# of selkies and coturn would otherwise not see these computed values)
env | sort | while IFS= read -r kv; do
  printf 'export %s=%q\n' "${kv%%=*}" "${kv#*=}"
done > "${ENV_FILE}"

# Derive the service set from the environment toggles
if [ "${SELKIES_WAYLAND:-false}" = "true" ]; then
  rm -rf /etc/service/xvfb /etc/service/lxqt 2>/dev/null || sudo-root rm -rf /etc/service/xvfb /etc/service/lxqt 2>/dev/null || true
  echo 'SELKIES_WAYLAND=true: headless Wayland compositor replaces Xvfb; no desktop session is started'
else
  if [ "${START_LXQT:-true}" != "true" ]; then
    rm -rf /etc/service/lxqt 2>/dev/null || sudo-root rm -rf /etc/service/lxqt 2>/dev/null || true
  fi
fi
if [ "${SELKIES_ENABLE_INTERNAL_TURN}" != "true" ]; then
  rm -rf /etc/service/coturn 2>/dev/null || sudo-root rm -rf /etc/service/coturn 2>/dev/null || true
fi

# Hand over to s6 service supervision (works as PID 1 or below any other init)
exec s6-svscan -t5 /etc/service
