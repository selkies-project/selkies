#!/bin/bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Dev setup for the pure-Rust pixelflux PyO3 extension.
set -euxo pipefail

sudo apt-get update
# System C libraries the crate links against (x264-sys -> libx264, codec-sys -> the
# software codec libraries it binds from their headers, x11rb -> libxcb + shm +
# xfixes, GBM/DRM, Wayland/xkb, pixman, and the libinput and libudev the compositor's
# crates link) plus the build toolchain (cmake and nasm build the vendored libjpeg-turbo
# source and nasm the vendored OpenH264, both statically linked and needing no system
# copy). The VA-API session opens libva at run time, so only its runtime package is
# needed.
sudo apt-get install -y \
  build-essential pkg-config cmake nasm clang libclang-dev curl ca-certificates \
  libx265-dev libvpx-dev libsvtav1enc-dev libdav1d-dev libde265-dev \
  libva2 libdrm-dev libgbm-dev libpixman-1-dev libinput-dev libudev-dev \
  libwayland-dev libxkbcommon-dev \
  libxcb1-dev libxcb-shm0-dev libxcb-xfixes0-dev \
  python3-dev python3-pip

# libx264 with the API pixelflux reads, which patches/x264 adds.
sudo sh "$(dirname "$0")/../scripts/build-x264.sh" /usr/local

# firefox-esr (for end-to-end testing the stream in a browser)
sudo apt install -y software-properties-common && sudo add-apt-repository ppa:mozillateam/ppa -y && sudo apt install -y firefox-esr

# Rust toolchain (the extension is built via setuptools-rust / cargo).
# rustup's installer, checked against the SHA-256 published beside it.
if ! command -v cargo >/dev/null 2>&1; then
  url="https://static.rust-lang.org/rustup/dist/$(uname -m)-unknown-linux-gnu/rustup-init"
  curl --proto '=https' --tlsv1.2 -sSf -o /tmp/rustup-init "${url}"
  sum="$(curl --proto '=https' --tlsv1.2 -sSf "${url}.sha256")"
  echo "${sum%% *}  /tmp/rustup-init" | sha256sum -c -
  chmod +x /tmp/rustup-init && /tmp/rustup-init -y && rm -f /tmp/rustup-init
  # shellcheck source=/dev/null
  source "$HOME/.cargo/env"
fi

# Build and install the extension from source, into the system Python the distribution
# marks externally managed: the container is the environment.
pip3 install --break-system-packages setuptools-rust
pip3 install --break-system-packages .
