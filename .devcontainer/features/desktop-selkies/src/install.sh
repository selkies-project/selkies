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

# Install Selkies from PyPI (latest release or a requested release tag)
if [ "${RELEASE:-latest}" = "latest" ]; then
    PIP_BREAK_SYSTEM_PACKAGES=1 pip3 install --no-cache-dir --upgrade selkies
else
    PIP_BREAK_SYSTEM_PACKAGES=1 pip3 install --no-cache-dir "selkies==${RELEASE#v}"
fi

mkdir -p /etc/OpenCL/vendors && echo "libnvidia-opencl.so.1" > /etc/OpenCL/vendors/nvidia.icd

# Copy turnserver script
cp start-turnserver.sh /usr/local/bin/start-turnserver.sh
chmod -f +x /usr/local/bin/start-turnserver.sh

# Copy the startup script
cp start-selkies.sh /usr/local/bin/start-selkies.sh
chmod -f +x /usr/local/bin/start-selkies.sh
