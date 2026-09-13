#!/bin/bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

set -e

echo "Activating feature 'Selkies'"
echo "The provided release version is: ${RELEASE:-missing env}"
echo "The provided web port is: ${WEB_PORT:-missing env}"
echo "The provided xserver is: ${XSERVER:-missing env}"
echo "The provided desktop is: ${DESKTOP:-missing env}"
echo "Use Wayland backend: ${WAYLAND:-false}"

export DEBIAN_FRONTEND="noninteractive"

# Retried for the same reason the devcontainer image retries: a feature install
# that reaches for the apt index or PyPI is one transient failure away from
# failing the whole container create. install-desktop-environment.sh, invoked
# below, inherits the drop-in.
printf 'Acquire::Retries "5";\nAcquire::http::Timeout "30";\nAcquire::https::Timeout "30";\nAcquire::Retries::Delay::Maximum "30";\n' > /etc/apt/apt.conf.d/99-selkies-retries

# Install base dependencies (X11 capture/input for pixelflux, PulseAudio for
# pcmflux, Mesa/VA-API for GPU acceleration, and the display stack)
apt-get clean && apt-get update && apt-get install --no-install-recommends -y \
    ca-certificates \
    curl \
    jq \
    python3-pip \
    python3-dev \
    python3-setuptools \
    python3-wheel \
    libgcrypt20 \
    libglib2.0-0 \
    glib-networking \
    libpulse0 \
    pulseaudio \
    pulseaudio-utils \
    libdrm2 \
    libegl1 \
    libgl1 \
    libgles2 \
    libglvnd0 \
    libglx0 \
    libva2 \
    libva-drm2 \
    libgbm1 \
    libpixman-1-0 \
    libxcb-render0 \
    mesa-utils \
    wayland-protocols \
    libwayland-egl1 \
    libxkbcommon0 \
    wmctrl \
    x11-utils \
    x11-xkb-utils \
    x11-xserver-utils \
    xserver-xorg-core \
    xvfb \
    libx11-xcb1 \
    libxcb1 \
    libxcb-shm0 \
    libxcb-xfixes0 \
    libxcb-randr0 \
    libxcb-dri3-0 \
    libxdamage1 \
    libxfixes3 \
    libxtst6 \
    libxext6 \
    coturn && \
apt-get clean && rm -rf /var/lib/apt/lists/* /var/cache/debconf/* /var/log/* /tmp/* /var/tmp/*

# Install desktop environment (LXQt), unless "none" was selected
if [ "${DESKTOP:-lxqt}" != "none" ]; then
    ./install-desktop-environment.sh
fi

# Install Selkies from the wheels its GitHub releases carry -- the release asked
# for, else the newest one, and the newest pixelflux and pcmflux releases --
# looked in ahead of the index, which only resolves what no release carries.
WHEELS="$(mktemp -d)"
./fetch-release-wheels.sh "${WHEELS}" "${RELEASE:-latest}"
# The capture stack goes in from the wheels ahead of the index the remaining
# dependencies come from, so its pin is met before any resolver looks there.
for project in pixelflux pcmflux; do
    if ls "${WHEELS}/${project}"-*.whl > /dev/null 2>&1; then
        PIP_BREAK_SYSTEM_PACKAGES=1 pip3 install --no-cache-dir --no-index --find-links "${WHEELS}" "${project}"
    fi
done
SELKIES="$(ls "${WHEELS}"/selkies-*.whl 2> /dev/null || echo "selkies${RELEASE:+==${RELEASE#v}}")"
PIP_BREAK_SYSTEM_PACKAGES=1 pip3 install --no-cache-dir --retries 5 --timeout 60 --find-links "${WHEELS}" "${SELKIES}"
rm -rf "${WHEELS}"

mkdir -p /etc/OpenCL/vendors && echo "libnvidia-opencl.so.1" > /etc/OpenCL/vendors/nvidia.icd

# Copy turnserver script
cp start-turnserver.sh /usr/local/bin/start-turnserver.sh
chmod -f +x /usr/local/bin/start-turnserver.sh

# Copy the startup script
cp start-selkies.sh /usr/local/bin/start-selkies.sh
chmod -f +x /usr/local/bin/start-selkies.sh
