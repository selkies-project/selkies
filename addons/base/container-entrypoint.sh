#!/bin/bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# PID-agnostic container init for the Selkies desktop container. It prepares the
# runtime environment (joystick, webcam and fake-udev LD_PRELOAD, device nodes,
# TURN defaults), derives the service set from the environment toggles, and then
# hands service supervision to s6 (`s6-svscan /etc/service`): one `s6-supervise`
# per service directory, restarts crashed services, and is controlled with
# `s6-svc`/`s6-svstat`. s6-svscan runs at any PID: it can be launched below an
# injected init or script, and equally works when this script itself is PID 1
# (in which case `docker run --init` for zombie reaping is recommended).

set -e

# A configured value as settings.py reads one: the field before any |suffix,
# trimmed of surrounding whitespace. Every reader has to agree on this rule or
# the same deployment is configured two ways at once.
setting_value() {
  local value="${1%%|*}"
  value="${value#"${value%%[![:space:]]*}"}"
  printf '%s' "${value%"${value##*[![:space:]]}"}"
}

# Mirrors settings.py parse_bool: "true" or "1", case-insensitively; anything
# else — a blank value included — is false.
is_true() {
  local value
  value="$(setting_value "$1")"
  value="${value,,}"
  [ "${value}" = "true" ] || [ "${value}" = "1" ]
}

# Wait for XDG_RUNTIME_DIR to exist (created by the image ENV or the caller)
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp/runtime-ubuntu}"
# The mode is set separately: mkdir applies -m only to a directory it creates,
# and a runtime directory left over from a previous run would keep whatever it
# had. The spec requires 0700 and dbus and PipeWire refuse anything wider.
mkdir -p "${XDG_RUNTIME_DIR}"
# Not fatal: a runtime directory bind-mounted from elsewhere may not be ours
# to re-mode, and that is no reason to refuse to start the container.
chmod 700 "${XDG_RUNTIME_DIR}" 2>/dev/null || true

# A desktop menu watches the directories it read at startup, and cannot watch one
# that does not exist yet: the first application installed into a home without
# them lands in a directory nothing is watching, and never reaches the running
# session's menu -- the second and every later one does, which is what makes it
# look like the application, rather than the home, is at fault. Creating them
# before the session starts is what keeps that first install visible.
for dir in applications icons/hicolor; do
  mkdir -p "${XDG_DATA_HOME:-${HOME}/.local/share}/${dir}" 2>/dev/null || \
    echo "selkies: cannot create ${XDG_DATA_HOME:-${HOME}/.local/share}/${dir}; a first install may not reach the menu" >&2
done

# Configure joystick interposer and fake-udev (container-only gamepad plumbing)
# $LIB is a dynamic-loader token; the backslash keeps the shell off it.
export LIB_PREFIX="/usr/\$LIB"
export SELKIES_INTERPOSER="${LIB_PREFIX}/selkies_joystick_interposer.so"
export LIBUDEV_PACKAGE="${LIBUDEV_PACKAGE:-libudev}"
export LIBUDEV_PKG_VERSION="${LIBUDEV_PKG_VERSION:-1.0.0}"
export FAKE_UDEV_LIB="${LIB_PREFIX}/${LIBUDEV_PACKAGE}.so.${LIBUDEV_PKG_VERSION}-fake"
# The webcam interposer serves the client's camera as /dev/video0 to session
# applications, with no kernel device and no privilege. The Selkies backend
# feeds it and must keep seeing real device nodes, so selkies-entrypoint.sh
# drops this entry again for its own process.
export SELKIES_WEBCAM_INTERPOSER="${LIB_PREFIX}/selkies_v4l2_interposer.so"
export LD_PRELOAD="${SELKIES_INTERPOSER}:${FAKE_UDEV_LIB}:${SELKIES_WEBCAM_INTERPOSER}${LD_PRELOAD:+:${LD_PRELOAD}}"
# No SDL_JOYSTICK_DEVICE: fake-udev enumerates all four slots as their evdev
# nodes, so a /dev/input/js0 hint would show slot 0 a second time.
# The interposer answers for the pads' nodes whether or not they exist and
# adds them to a listing of /dev/input, so no device files are made; only the
# directory has to exist for a listing to be augmented. Only when the container
# has none of its own: a real one passed in from the host keeps its own
# ownership and mode.
if [ ! -d /dev/input ]; then
  # shellcheck disable=SC2174  # /dev always exists, so -m applies to the leaf
  mkdir -pm1777 /dev/input || sudo-root mkdir -pm1777 /dev/input || echo 'Failed to create the /dev/input directory the joystick interposer lists into'
fi

# Backend switch, in settings.py's own order: SELKIES_WAYLAND first, the legacy
# PIXELFLUX_WAYLAND after, and a set-but-blank variable counting as set. The
# service set below would otherwise start an X11 session for a Wayland capture.
if is_true "${SELKIES_WAYLAND-${PIXELFLUX_WAYLAND-false}}"; then
  export SELKIES_WAYLAND="true"
else
  export SELKIES_WAYLAND="false"
fi

# Transport mode, canonicalized the way settings.py reads its enum values, so
# the service set below and selkies itself see the same choice. Left unset
# when not configured: the settings default applies.
SELKIES_MODE="$(setting_value "${SELKIES_MODE-}")"
SELKIES_MODE="${SELKIES_MODE,,}"
if [ -n "${SELKIES_MODE}" ]; then
  export SELKIES_MODE
fi

# What GPU this session has. selkies-gpu-probe resolves the render node from
# --render-dri and the --auto-gpu token exactly as the capture will, and carries
# the compositor's own renderer bring-up through on it, so the GL stack and the
# backend below follow one answer rather than each guessing from device paths.
gpu_status=0
gpu_facts="$(timeout 60 selkies-gpu-probe)" || gpu_status=$?
while IFS= read -r assignment; do
  # Assignments with a clean name only, so a tool that answers something else
  # cannot be exported into this environment or abort it under set -e.
  case "${assignment}" in *=*) ;; *) continue ;; esac
  case "${assignment%%=*}" in
    ''|*[!A-Z0-9_]*) ;;
    *) export "${assignment?}" ;;
  esac
done <<EOF
${gpu_facts}
EOF
# 124 and up is timeout's own status or a signal: the bring-up wedged or died on
# this driver stack, which the session would then do on every restart. A report
# that merely could not be had exits 1 and settles nothing.
if [ "${gpu_status}" -ge 124 ]; then
  echo "GPU: the renderer bring-up did not survive this driver stack (status ${gpu_status})"
  export SELKIES_GPU_PRESENT="true"
  export SELKIES_GPU_ACCELERATED="false"
fi
# With no report at all, the driver's own devices are the only signal left that
# this is the NVIDIA stack.
if [ -z "${SELKIES_GPU_DRIVER-}" ] && ls /dev/nvidia* >/dev/null 2>&1 && nvidia-smi >/dev/null 2>&1; then
  export SELKIES_GPU_DRIVER="nvidia"
fi

# Hardware OpenGL for the session's applications. Mesa carries no driver for the
# proprietary NVIDIA stack, so GL there runs through Zink on its Vulkan driver,
# which needs no render node of its own; every other vendor has a native Mesa
# driver, and forcing Zink on it would aim Mesa at a device the session does not
# render on. DISABLE_ZINK=true leaves that GPU to software OpenGL.
if [ "${SELKIES_GPU_DRIVER-}" != "nvidia" ]; then
  :
elif is_true "${DISABLE_ZINK-false}"; then
  echo 'DISABLE_ZINK is set: OpenGL is not routed through Zink on the NVIDIA GPU'
else
  export LIBGL_KOPPER_DRI2="1"
  export MESA_LOADER_DRIVER_OVERRIDE="zink"
  export GALLIUM_DRIVER="zink"
  echo 'NVIDIA GPU: OpenGL runs through Zink on the NVIDIA Vulkan driver'
fi

# A GPU the compositor cannot reach is a reason to run X11 instead, where the
# session still gets it through Zink or the X server's own render node
# (services/xvfb/run); with no GPU at all both backends render in software and
# switching would trade a capability for nothing. Settled before anything
# derived from the backend: the display, the session type and the toolkit
# defaults all follow it. SELKIES_WAYLAND_X11_FALLBACK=false keeps Wayland and
# composites in software, which shares no dmabuf, so a GL client aimed at the
# Vulkan driver would produce buffers it cannot accept — the Zink override goes
# with it.
if [ "${SELKIES_WAYLAND}" = "true" ] && [ "${SELKIES_GPU_PRESENT-}" = "true" ] \
   && [ "${SELKIES_GPU_ACCELERATED-}" = "false" ]; then
  if is_true "${SELKIES_WAYLAND_X11_FALLBACK-true}"; then
    echo 'GPU: the compositor cannot reach it; starting X11 so the session keeps it'
    export SELKIES_WAYLAND="false"
  else
    echo 'GPU: the compositor cannot reach it; compositing in software'
    unset GALLIUM_DRIVER MESA_LOADER_DRIVER_OVERRIDE LIBGL_KOPPER_DRI2
  fi
fi

# With no DRM render node, the NVIDIA EGL/GBM vendor library the container
# runtime injects segfaults inside Xwayland's EGL init, and glamor would have
# nothing to accelerate anyway. wlroots honors WLR_XWAYLAND, so XWayland is
# started through a wrapper pinning EGL to Mesa and shared-memory buffers.
if [ "${SELKIES_WAYLAND}" = "true" ] && ! ls /dev/dri/renderD* > /dev/null 2>&1; then
  printf '#!/bin/sh\nexport __EGL_VENDOR_LIBRARY_FILENAMES=/usr/share/glvnd/egl_vendor.d/50_mesa.json\nexec /usr/bin/Xwayland -shm "$@"\n' > /tmp/selkies-xwayland
  chmod 755 /tmp/selkies-xwayland
  export WLR_XWAYLAND="/tmp/selkies-xwayland"
fi

# Default display for the X11 backend. In Wayland mode the session compositor's
# XWayland server owns it instead, and takes the first free number.
if [ "${SELKIES_WAYLAND}" = "true" ]; then
  export DISPLAY="${DISPLAY:-:0}"
  # Session compositor for the Wayland backend: it nests inside the capture
  # compositor, which provides neither window management nor XWayland. "none"
  # leaves applications on the capture compositor — Wayland clients only,
  # unmanaged. Lowercased because every value, "none" included, is lower case.
  SELKIES_WAYLAND_COMPOSITOR="$(setting_value "${SELKIES_WAYLAND_COMPOSITOR-}")"
  SELKIES_WAYLAND_COMPOSITOR="${SELKIES_WAYLAND_COMPOSITOR,,}"
  if [ -z "${SELKIES_WAYLAND_COMPOSITOR}" ]; then
    # A WAYLAND_DISPLAY set before this script ran names an operator's own
    # session (the capture display is exported later), so capture it rather
    # than nest in it; connect-probed, since a stale socket file outlives its
    # compositor. Otherwise labwc, which decorates windows and honors the
    # maximize and minimize a Wayland taskbar sends. An absolute
    # WAYLAND_DISPLAY is legal, and prefixing it would build a bogus path.
    case "${WAYLAND_DISPLAY-}" in
      /*) _wl_sock="${WAYLAND_DISPLAY}" ;;
      *)  _wl_sock="${XDG_RUNTIME_DIR}/${WAYLAND_DISPLAY-}" ;;
    esac
    if [ -n "${SELKIES_WAYLAND_HOST_DISPLAY-}" ]; then
      # An operator naming the host display asks for host capture outright:
      # that compositor owns the session, so nesting one here is wrong.
      echo "SELKIES_WAYLAND_HOST_DISPLAY=${SELKIES_WAYLAND_HOST_DISPLAY}: capturing the named compositor"
      export SELKIES_WAYLAND_HOST_DISPLAY
      SELKIES_WAYLAND_COMPOSITOR=none
    elif [ -n "${WAYLAND_DISPLAY-}" ] \
       && [ -S "${_wl_sock}" ] \
       && python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.close()' \
            "${_wl_sock}" 2>/dev/null; then
      echo "Wayland compositor detected at ${_wl_sock}: capturing it directly"
      export SELKIES_WAYLAND_HOST_DISPLAY="${_wl_sock}"
      SELKIES_WAYLAND_COMPOSITOR=none
    else
      command -v labwc >/dev/null 2>&1 && SELKIES_WAYLAND_COMPOSITOR=labwc
    fi
    unset _wl_sock
  elif [ "${SELKIES_WAYLAND_COMPOSITOR}" != "none" ] && ! command -v "${SELKIES_WAYLAND_COMPOSITOR}" >/dev/null 2>&1; then
    echo "SELKIES_WAYLAND_COMPOSITOR=${SELKIES_WAYLAND_COMPOSITOR} is not installed; applications stay on the capture compositor"
    SELKIES_WAYLAND_COMPOSITOR=none
  fi
  export SELKIES_WAYLAND_COMPOSITOR="${SELKIES_WAYLAND_COMPOSITOR:-none}"
  # Toolkits that can speak both protocols prefer Wayland and keep X11 as the
  # fallback; Selkies resolves WAYLAND_DISPLAY itself, per compositor socket.
  export XDG_SESSION_TYPE="wayland"
  export GDK_BACKEND="${GDK_BACKEND:-wayland,x11}"
  export MOZ_ENABLE_WAYLAND="${MOZ_ENABLE_WAYLAND:-1}"
else
  export DISPLAY="${DISPLAY:-:20}"
fi
# Every menu-reading application, not only the session's own: without the
# prefix libfm-qt searches for a bare applications.menu, finds none, and shows
# an empty application menu. Applications the dashboard launches are children
# of the selkies service rather than of the session, so it belongs here, where
# the shared environment is computed, and not in the session's script.
export XDG_MENU_PREFIX="${XDG_MENU_PREFIX:-lxqt-}"
# PipeWire audio latency and the PipeWire-Pulse server socket path
export PIPEWIRE_LATENCY="${PIPEWIRE_LATENCY:-256/48000}"
export PIPEWIRE_RUNTIME_DIR="${PIPEWIRE_RUNTIME_DIR:-${XDG_RUNTIME_DIR}}"
export PULSE_RUNTIME_PATH="${PULSE_RUNTIME_PATH:-${XDG_RUNTIME_DIR}/pulse}"
export PULSE_SERVER="${PULSE_SERVER:-unix:${PULSE_RUNTIME_PATH}/native}"

# Compute the shared session environment, including embedded coTURN defaults
ENV_FILE="${XDG_RUNTIME_DIR}/container-env"
: > "${ENV_FILE}"

# The address a browser outside this container would reach it on, asked of a
# public resolver as the only party that can see it; an IPv6 answer is bracketed
# for a URL. Falls back to the container's own address, which serves a LAN.
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

# Whether an external TURN server is configured well enough to be used at all: a
# REST service, or a host and port with either credentials or a shared secret.
# Anything short of that and the container runs its own.
external_turn_configured() {
  [ -n "${SELKIES_TURN_REST_URI}" ] && return 0
  [ -n "${SELKIES_TURN_HOST}" ] && [ -n "${SELKIES_TURN_PORT}" ] || return 1
  [ -n "${SELKIES_TURN_SHARED_SECRET}" ] && return 0
  [ -n "${SELKIES_TURN_USERNAME}" ] && [ -n "${SELKIES_TURN_PASSWORD}" ]
}

export SELKIES_ENABLE_INTERNAL_TURN="false"
if [ "${SELKIES_MODE}" = "webrtc" ] || is_true "${SELKIES_ENABLE_DUAL_MODE-false}"; then
  if ! external_turn_configured; then
    export SELKIES_ENABLE_INTERNAL_TURN="true"
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
# of selkies and coturn would otherwise not see these computed values). The
# variables a shell maintains for itself are left out: exported into another
# shell they describe that shell wrongly.
env | sort | while IFS= read -r kv; do
  case "${kv%%=*}" in
    PWD|OLDPWD|SHLVL|_|PS1|BASH_*) continue ;;
  esac
  printf 'export %s=%q\n' "${kv%%=*}" "${kv#*=}"
done > "${ENV_FILE}"

# Derive the service set from the environment toggles, holding services down
# rather than deleting them and releasing every one first. A restarted container
# keeps its filesystem, so a deleted service would be gone from the image copy for
# good, past any later change of SELKIES_WAYLAND or of what is installed.
drop_service() {
  [ -d "/etc/service/$1" ] || return 0
  { : > "/etc/service/$1/down"; } 2>/dev/null ||
    sudo-root sh -c ": > '/etc/service/$1/down'" 2>/dev/null || true
}

for service in /etc/service/*/; do
  rm -f "${service}down" 2>/dev/null || sudo-root rm -f "${service}down" 2>/dev/null || true
done

if [ "${SELKIES_WAYLAND}" = "true" ]; then
  drop_service xvfb
  if [ "${SELKIES_WAYLAND_COMPOSITOR}" = "none" ]; then
    # No nested session compositor either way: host capture leaves window
    # management (and any XWayland) to the operator's own compositor, while
    # a bare none leaves applications on the capture compositor with neither.
    drop_service wayland
    drop_service lxqt
    if [ -n "${SELKIES_WAYLAND_HOST_DISPLAY-}" ]; then
      echo "Wayland backend: capturing the operator's compositor at ${SELKIES_WAYLAND_HOST_DISPLAY}; it keeps its own window management"
    else
      echo 'Wayland backend: applications connect to the capture compositor directly; no XWayland'
    fi
  else
    echo "Wayland backend: ${SELKIES_WAYLAND_COMPOSITOR} nests inside the capture compositor and provides XWayland"
  fi
else
  drop_service wayland
fi
START_LXQT="$(setting_value "${START_LXQT-}")"
is_true "${START_LXQT:-true}" || drop_service lxqt
if ! is_true "${SELKIES_ENABLE_INTERNAL_TURN}"; then
  drop_service coturn
fi

# Hand over to s6 service supervision. -t is a rescan interval in milliseconds,
# so this is the five seconds daemontools used, not five of them.
exec s6-svscan -t5000 /etc/service
