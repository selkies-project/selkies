#!/bin/bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# PID-agnostic container init for the Selkies example container. It prepares the
# runtime environment (joystick interposer + fake-udev LD_PRELOAD, device nodes,
# TURN defaults), derives the service set from the environment toggles, and then
# hands service supervision to s6 (`s6-svscan /etc/service`): one `s6-supervise`
# per service directory, restarts crashed services, and is controlled with
# `s6-svc`/`s6-svstat`. s6-svscan runs at any PID: it can be launched below an
# injected init or script, and equally works when this script itself is PID 1
# (in which case `docker run --init` for zombie reaping is recommended).

set -e

# A configured value's truth, read exactly as selkies.settings.parse_bool reads it:
# "true" or "1", case-insensitively, ahead of any "|locked" suffix. Environment
# variables carry whatever case the operator typed, and a stricter reading here would
# configure the container one way and the server it starts another.
is_true() {
  local value="${1%%|*}"
  value="${value,,}"
  value="${value//[[:space:]]/}"
  [ "${value}" = "true" ] || [ "${value}" = "1" ]
}

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

# The interposer's device nodes, made directly where this container runs privileged
# enough and through the image's setuid helper where it does not. Best effort: a
# session without them loses gamepad support and nothing else.
make_node() {
  mknod "$1" c "$2" "$3" || sudo-root mknod "$1" c "$2" "$3" || echo "Failed to create device file $1"
}

if [ -d /dev/input ]; then
  for i in 0 1 2 3; do
    make_node "/dev/input/js${i}" 13 "${i}"
    make_node "/dev/input/event100${i}" 13 "106${i}"
  done
  chmod 0666 /dev/input/js* /dev/input/event* ||
    sudo-root chmod 0666 /dev/input/js* /dev/input/event* ||
    echo 'Failed to change permission for joystick interposer devices'
else
  echo 'Skipping joystick interposer device files creation since /dev/input is unavailable'
fi

# Backend switch. Selkies resolves SELKIES_WAYLAND first and falls back to
# PIXELFLUX_WAYLAND, so the service set below has to follow the same order or
# the container would start an X11 session for a Wayland capture.
if is_true "${SELKIES_WAYLAND:-${PIXELFLUX_WAYLAND:-false}}"; then
  export SELKIES_WAYLAND=true
else
  export SELKIES_WAYLAND=false
fi

# Hardware OpenGL. On NVIDIA, Zink routes GL through the Vulkan driver; other
# vendors reach the GPU through the display server's render node instead (see
# services/xvfb/run). Both signals are required: the device nodes prove a GPU was
# passed in, and a working nvidia-smi proves the driver stack matches it. Set
# DISABLE_ZINK=true for llvmpipe. Settled before the backend is, so the probe
# below sees the GL environment the session will actually run with.
if ! is_true "${DISABLE_ZINK:-false}" && ls /dev/nvidia* >/dev/null 2>&1 && nvidia-smi >/dev/null 2>&1; then
  export LIBGL_KOPPER_DRI2=1
  export MESA_LOADER_DRIVER_OVERRIDE=zink
  export GALLIUM_DRIVER=zink
  echo 'NVIDIA GPU detected: OpenGL runs through Zink on the NVIDIA Vulkan driver'
fi

# A GPU the Wayland session cannot reach is a reason to run X11 instead: under Xvfb the
# session still gets it (through Zink on NVIDIA, or the server's own render node
# elsewhere), while a compositor with no working GBM/EGL stack composites in software.
# selkies-gpu-probe weighs that up and names the backend, printing its reason; it stays
# silent about the backend when no report can be had (an older pixelflux, a driver that
# refuses to answer), which leaves the session exactly as it was asked for.
if [ "${SELKIES_WAYLAND}" = "true" ]; then
  case "$(timeout 60 selkies-gpu-probe || true)" in
    x11) export SELKIES_WAYLAND=false ;;
    # A compositor rendering in software shares no dmabuf, so a GL client aimed at
    # the Vulkan driver produces buffers it cannot accept and draws nothing.
    wayland-software) unset GALLIUM_DRIVER MESA_LOADER_DRIVER_OVERRIDE LIBGL_KOPPER_DRI2 ;;
  esac
fi

# Default display for the X11 backend. In Wayland mode the session compositor's
# XWayland server owns it instead, and takes the first free number.
if [ "${SELKIES_WAYLAND}" = "true" ]; then
  export DISPLAY="${DISPLAY:-:0}"
  # Session compositor for the Wayland backend: it nests inside the capture
  # compositor to add window management and XWayland, neither of which the
  # capture compositor provides. "none" keeps applications on the capture
  # compositor alone — Wayland clients only, unmanaged.
  # Lowercased so the value reads the same however it was typed: every compositor
  # binary and the "none" sentinel are spelled in lower case.
  SELKIES_WAYLAND_COMPOSITOR="${SELKIES_WAYLAND_COMPOSITOR,,}"
  if [ -z "${SELKIES_WAYLAND_COMPOSITOR}" ]; then
    # sway, whose `create_output` is what lets a second display appear and leave
    # while the session runs. Another compositor can be named, but only sway
    # tracks the capture outputs.
    for wm in sway; do
      command -v "${wm}" >/dev/null 2>&1 && SELKIES_WAYLAND_COMPOSITOR="${wm}" && break
    done
  elif [ "${SELKIES_WAYLAND_COMPOSITOR}" != "none" ] && ! command -v "${SELKIES_WAYLAND_COMPOSITOR}" >/dev/null 2>&1; then
    echo "SELKIES_WAYLAND_COMPOSITOR=${SELKIES_WAYLAND_COMPOSITOR} is not installed; applications stay on the capture compositor"
    SELKIES_WAYLAND_COMPOSITOR=none
  fi
  export SELKIES_WAYLAND_COMPOSITOR="${SELKIES_WAYLAND_COMPOSITOR:-none}"
  # Toolkits that can speak both protocols prefer Wayland and keep X11 as the
  # fallback; Selkies resolves WAYLAND_DISPLAY itself, per compositor socket.
  export XDG_SESSION_TYPE=wayland
  export GDK_BACKEND="${GDK_BACKEND:-wayland,x11}"
  export MOZ_ENABLE_WAYLAND="${MOZ_ENABLE_WAYLAND:-1}"
else
  export DISPLAY="${DISPLAY:-:20}"
fi
# PipeWire-Pulse server socket path
export PIPEWIRE_LATENCY="128/48000"
export PIPEWIRE_RUNTIME_DIR="${PIPEWIRE_RUNTIME_DIR:-${XDG_RUNTIME_DIR}}"
export PULSE_RUNTIME_PATH="${PULSE_RUNTIME_PATH:-${XDG_RUNTIME_DIR}/pulse}"
export PULSE_SERVER="${PULSE_SERVER:-unix:${PULSE_RUNTIME_PATH}/native}"

# Compute the shared session environment, including embedded coTURN defaults
ENV_FILE="${XDG_RUNTIME_DIR}/container-env"
: > "${ENV_FILE}"

# The address a browser outside this container would reach it on. Asked of a public
# resolver, which is the only party that can see it; an IPv6 answer is bracketed for
# use in a URL. Falls back to the container's own address, which is right for a LAN
# and at least routable for a local test.
public_address() {
  local answer
  for family in -4 -6; do
    answer="$(dig "${family}" TXT +short @ns1.google.com o-o.myaddr.l.google.com 2>/dev/null \
              | tr -d '"' | grep -v '^;;' | head -n 1)"
    [ -z "${answer}" ] && continue
    [ "${family}" = "-6" ] && answer="[${answer}]"
    echo "${answer}"
    return
  done
  answer="$(hostname -I 2>/dev/null | awk '{print $1; exit}')"
  echo "${answer:-127.0.0.1}"
}

# coTURN binds an address, not a name, so a hostname has to be resolved for it.
resolved_address() {
  local host answer
  host="$(echo "$1" | tr -d '[]')"
  answer="$(getent ahostsv4 "${host}" 2>/dev/null | awk '{print $1; exit}')"
  if [ -z "${answer}" ]; then
    answer="$(getent ahostsv6 "${host}" 2>/dev/null | awk '{print "[" $1 "]"; exit}')"
  fi
  echo "${answer}"
}

# Whether an external TURN server is configured well enough to be used: a REST service
# to fetch credentials from, or a host and port together with credentials — a username
# and password, or a shared secret to derive them from. Anything short of that and the
# container runs its own, since a half-configured TURN server is no TURN server.
external_turn_configured() {
  [ -n "${SELKIES_TURN_REST_URI}" ] && return 0
  [ -n "${SELKIES_TURN_HOST}" ] && [ -n "${SELKIES_TURN_PORT}" ] || return 1
  [ -n "${SELKIES_TURN_SHARED_SECRET}" ] && return 0
  [ -n "${SELKIES_TURN_USERNAME}" ] && [ -n "${SELKIES_TURN_PASSWORD}" ]
}

export SELKIES_ENABLE_INTERNAL_TURN=false
if [ "${SELKIES_MODE,,}" = "webrtc" ] || is_true "${SELKIES_ENABLE_DUAL_MODE:-false}"; then
  if ! external_turn_configured; then
    export SELKIES_ENABLE_INTERNAL_TURN=true
    TURN_RANDOM_PASSWORD="$(tr -dc 'A-Za-z0-9' < /dev/urandom 2>/dev/null | head -c 24)"
    export TURN_RANDOM_PASSWORD
    export SELKIES_TURN_HOST="${SELKIES_TURN_HOST:-$(public_address)}"
    export TURN_EXTERNAL_IP="${TURN_EXTERNAL_IP:-$(resolved_address "${SELKIES_TURN_HOST}")}"
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
# Held down rather than deleted, and every service is released first, so the set is
# derived from the environment on each start. A container that is stopped and started
# again keeps its filesystem: a service removed here would be gone from the image copy
# for good, and no later change of SELKIES_WAYLAND or of which compositors are installed
# could bring it back.
write_service_flag() {
  { : > "/etc/service/$1/down"; } 2>/dev/null ||
    sudo-root sh -c ": > '/etc/service/$1/down'" 2>/dev/null || true
}

drop_service() {
  [ -d "/etc/service/$1" ] || return 0
  write_service_flag "$1"
}

for service in /etc/service/*/; do
  rm -f "${service}down" 2>/dev/null || sudo-root rm -f "${service}down" 2>/dev/null || true
done

if [ "${SELKIES_WAYLAND}" = "true" ]; then
  drop_service xvfb
  if [ "${SELKIES_WAYLAND_COMPOSITOR}" = "none" ]; then
    # No session compositor means no window management and no XWayland, so a
    # desktop session cannot run either.
    drop_service wayland
    drop_service lxqt
    echo 'Wayland backend: applications connect to the capture compositor directly; no XWayland'
  else
    echo "Wayland backend: ${SELKIES_WAYLAND_COMPOSITOR} nests inside the capture compositor and provides XWayland"
  fi
else
  drop_service wayland
fi
is_true "${START_LXQT:-true}" || drop_service lxqt
if ! is_true "${SELKIES_ENABLE_INTERNAL_TURN}"; then
  drop_service coturn
fi

# Hand over to s6 service supervision
exec s6-svscan -t5 /etc/service
