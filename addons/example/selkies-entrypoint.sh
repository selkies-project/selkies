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

# Set default display
export DISPLAY="${DISPLAY:-:20}"
# PipeWire-Pulse server socket path
export PIPEWIRE_LATENCY="128/48000"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp}"
export PIPEWIRE_RUNTIME_DIR="${PIPEWIRE_RUNTIME_DIR:-${XDG_RUNTIME_DIR:-/tmp}}"
export PULSE_RUNTIME_PATH="${PULSE_RUNTIME_PATH:-${XDG_RUNTIME_DIR:-/tmp}/pulse}"
export PULSE_SERVER="${PULSE_SERVER:-unix:${PULSE_RUNTIME_PATH:-${XDG_RUNTIME_DIR:-/tmp}/pulse}/native}"

export SELKIES_ENCODER="${SELKIES_ENCODER:-x264enc}"
export SELKIES_ENABLE_RESIZE="${SELKIES_ENABLE_RESIZE:-false}"
if [ -z "${SELKIES_TURN_REST_URI}" ] && { { [ -z "${SELKIES_TURN_USERNAME}" ] || [ -z "${SELKIES_TURN_PASSWORD}" ]; } && [ -z "${SELKIES_TURN_SHARED_SECRET}" ] || [ -z "${SELKIES_TURN_HOST}" ] || [ -z "${SELKIES_TURN_PORT}" ]; }; then
  export TURN_RANDOM_PASSWORD="$(tr -dc 'A-Za-z0-9' < /dev/urandom 2>/dev/null | head -c 24)"
  export SELKIES_TURN_HOST="${SELKIES_TURN_HOST:-$(dig -4 TXT +short @ns1.google.com o-o.myaddr.l.google.com 2>/dev/null | { read output; if [ -z "$output" ] || echo "$output" | grep -q '^;;'; then exit 1; else echo "$(echo $output | sed 's,\",,g')"; fi } || dig -6 TXT +short @ns1.google.com o-o.myaddr.l.google.com 2>/dev/null | { read output; if [ -z "$output" ] || echo "$output" | grep -q '^;;'; then exit 1; else echo "[$(echo $output | sed 's,\",,g')]"; fi } || hostname -I 2>/dev/null | awk '{print $1; exit}' || echo '127.0.0.1')}"
  export TURN_EXTERNAL_IP="${TURN_EXTERNAL_IP:-$(getent ahostsv4 $(echo ${SELKIES_TURN_HOST} | tr -d '[]') 2>/dev/null | awk '{print $1; exit}' || getent ahostsv6 $(echo ${SELKIES_TURN_HOST} | tr -d '[]') 2>/dev/null | awk '{print "[" $1 "]"; exit}')}"
  export SELKIES_TURN_PORT="${SELKIES_TURN_PORT:-3478}"
  export SELKIES_TURN_USERNAME="selkies"
  export SELKIES_TURN_PASSWORD="${TURN_RANDOM_PASSWORD}"
  export SELKIES_TURN_PROTOCOL="${SELKIES_TURN_PROTOCOL:-tcp}"
  export SELKIES_STUN_HOST="${SELKIES_STUN_HOST:-stun.l.google.com}"
  export SELKIES_STUN_PORT="${SELKIES_STUN_PORT:-19302}"
  /etc/start-turnserver.sh &
fi

# Extract NVRTC dependency, https://developer.download.nvidia.com/compute/cuda/redist/cuda_nvrtc/LICENSE.txt
if command -v nvidia-smi &> /dev/null && nvidia-smi >/dev/null 2>&1; then
  NVRTC_DEST_PREFIX="${NVRTC_DEST_PREFIX-/usr}"
  CUDA_DRIVER_SYSTEM="$(nvidia-smi --version | grep 'CUDA Version' | cut -d: -f2 | tr -d ' ')"
  NVRTC_ARCH="${NVRTC_ARCH-$(dpkg --print-architecture | sed -e 's/arm64/sbsa/' -e 's/ppc64el/ppc64le/' -e 's/i.*86/x86/' -e 's/amd64/x86_64/' -e 's/unknown/x86_64/')}"
  # TEMPORARY: Cap CUDA version to 12.9 if the detected version is 13.0 or higher for NVRTC compatibility
  if [ -n "${CUDA_DRIVER_SYSTEM}" ]; then
    CUDA_MAJOR_VERSION=$(echo "${CUDA_DRIVER_SYSTEM}" | cut -d. -f1)
    if [ "${CUDA_MAJOR_VERSION}" -ge 13 ]; then
      CUDA_DRIVER_SYSTEM="12.9"
    fi
  fi
  NVRTC_URL="https://developer.download.nvidia.com/compute/cuda/redist/cuda_nvrtc/linux-${NVRTC_ARCH}/"
  NVRTC_ARCHIVE="$(curl -fsSL "${NVRTC_URL}" | grep -oP "(?<=href=')cuda_nvrtc-linux-${NVRTC_ARCH}-${CUDA_DRIVER_SYSTEM}\.[0-9]+-archive\.tar\.xz" | sort -V | tail -n 1)"
  if [ -z "${NVRTC_ARCHIVE}" ]; then
    FALLBACK_VERSION="${CUDA_DRIVER_SYSTEM}.0"
    NVRTC_ARCHIVE=$((curl -fsSL "${NVRTC_URL}" | grep -oP "(?<=href=')cuda_nvrtc-linux-${NVRTC_ARCH}-.*?\.tar\.xz" ; \
    echo "cuda_nvrtc-linux-${NVRTC_ARCH}-${FALLBACK_VERSION}-archive.tar.xz") | \
    sort -V | grep -B 1 --fixed-strings "${FALLBACK_VERSION}" | head -n 1)
  fi
  if [ -z "${NVRTC_ARCHIVE}" ]; then
      echo "ERROR: Could not find a compatible NVRTC archive." >&2
  fi
  echo "Selected NVRTC archive: ${NVRTC_ARCHIVE}"
  NVRTC_LIB_ARCH="$(dpkg --print-architecture | sed -e 's/arm64/aarch64-linux-gnu/' -e 's/armhf/arm-linux-gnueabihf/' -e 's/riscv64/riscv64-linux-gnu/' -e 's/ppc64el/powerpc64le-linux-gnu/' -e 's/s390x/s390x-linux-gnu/' -e 's/i.*86/i386-linux-gnu/' -e 's/amd64/x86_64-linux-gnu/' -e 's/unknown/x86_64-linux-gnu/')"
  cd /tmp && curl -fsSL "${NVRTC_URL}${NVRTC_ARCHIVE}" | tar -xJf - -C /tmp && mv -f cuda_nvrtc* cuda_nvrtc && cd cuda_nvrtc/lib && chmod -f 755 libnvrtc* && rm -f "${NVRTC_DEST_PREFIX}/lib/${NVRTC_LIB_ARCH}/"libnvrtc* && mv -f libnvrtc* "${NVRTC_DEST_PREFIX}/lib/${NVRTC_LIB_ARCH}/" && cd /tmp && rm -rf /tmp/cuda_nvrtc && cd "${HOME}"
fi

# Wait for X server to start
echo 'Waiting for X Socket' && until [ -S "/tmp/.X11-unix/X${DISPLAY#*:}" ]; do sleep 0.5; done && echo 'X Server is ready'

addr="0.0.0.0"

port="${SELKIES_PORT:-8080}"

# Setup dev mode if defined
if [ ! -z "${DEV_MODE+x}" ]; then
  # Frontend setup
  if [[ "${DEV_MODE}" == "core" ]]; then
    # Core just runs from directory
    cd $HOME/selkies/addons/selkies-web-core
    npm install
    npm run serve &
  else
    # Build core
    mkdir -p /opt/selkies-web/src
    # Define the dist-packages path for selkies_web
    SELKIES_WEB_DIST="/home/${USER}/selkies/src/selkies/selkies_web"
    mkdir -p "${SELKIES_WEB_DIST}/src"
    cp /opt/selkies-web/icon.png /opt/selkies-web/manifest.json ${SELKIES_WEB_DIST}

    cd $HOME/selkies/addons/selkies-web-core
    npm install
    npm run build
    echo ${SELKIES_WEB_DIST}/src ../${DEV_MODE}/src/ | xargs -n 1 cp dist/selkies-core.js 
    mkdir -p ../${DEV_MODE}/dist/assets/
    sudo nodemon --watch selkies-core.js \
                 --watch selkies-wr-core.js \
                 --watch selkies-ws-core.js --exec "npm run build && \
                 echo ../${DEV_MODE}/src/ ${SELKIES_WEB_DIST}/src/ | xargs -n 1 cp dist/selkies-core.js && \
                 cp dist/clipboard-worker* ../${DEV_MODE}/dist/assets/" &

    # Copy touch gamepad
    cp ../universal-touch-gamepad/universalTouchGamepad.js /opt/selkies-web/src/
    sudo nodemon --watch ../universal-touch-gamepad/universalTouchGamepad.js \
      --exec "echo /opt/selkies-web/src/ ${SELKIES_WEB_DIST}/src/ | \
      xargs -n 1 cp ../universal-touch-gamepad/universalTouchGamepad.js" &

    cd $HOME/selkies/addons/${DEV_MODE}
    npm install
    npm run build
    cp ../selkies-web-core/dist/clipboard-worker* dist/assets/
    cp -r dist/* /opt/selkies-web/
    sudo nodemon --watch ../${DEV_MODE}/src --exec "npm run build && \
      cp -r ../${DEV_MODE}/dist/* /opt/selkies-web/ && \
      cp ../selkies-web-core/dist/clipboard-worker* /opt/selkies-web/assets/ && \
      cp -r ../${DEV_MODE}/dist/* ${SELKIES_WEB_DIST}/ && \
      cp ../selkies-web-core/dist/clipboard-worker* ${SELKIES_WEB_DIST}/assets/" &
  fi

  # Run backend
  cd $HOME/selkies/src/
  nodemon -V --ext py --exec \
    "python3" -m selkies \
      --addr="${addr}" \
      --port="${port}" \
      --enable-basic-auth="false" \
      --mode="${SELKIES_MODE:-websockets}"
else
  # Start Selkies
  exec selkies \
    --addr="${addr}" \
    --port="${port}" \
    $@
fi

read