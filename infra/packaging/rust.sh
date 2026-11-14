#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Install the Rust toolchain the server is built with, the one pin every build
# of it reads: the bundle's (infra/packaging/server.sh, and the toolchain image
# that carries it, infra/bundle/Dockerfile) and the checks CI runs outside one
# (scripts/ci/server-build-deps.sh reads PIN from here). rustup-init
# is checked against the hash rustup publishes beside it and installs into
# RUSTUP_HOME and CARGO_HOME, by default under /opt/rust; where the pinned
# toolchain is already the default nothing is installed, and where rustup is
# there with another it adds the pin and makes it the default.
#
# Usage: rust.sh   (RUST_TOOLCHAIN overrides the pin)
set -eux
PIN="1.99.1"
RUST_TOOLCHAIN="${RUST_TOOLCHAIN:-${PIN}}"
export RUSTUP_HOME="${RUSTUP_HOME:-/opt/rust/rustup}" CARGO_HOME="${CARGO_HOME:-/opt/rust/cargo}"
export PATH="${CARGO_HOME}/bin:${PATH}"
if command -v rustc > /dev/null && rustc --version | grep -q "^rustc ${RUST_TOOLCHAIN} "; then
    exit 0
fi
if command -v rustup > /dev/null; then
    rustup toolchain install "${RUST_TOOLCHAIN}" --profile minimal
    rustup default "${RUST_TOOLCHAIN}"
    exit 0
fi
libc=gnu
if ldd --version 2>&1 | grep -qi musl; then libc=musl; fi
url="https://static.rust-lang.org/rustup/dist/$(uname -m)-unknown-linux-${libc}/rustup-init"
curl -fsSL --retry 5 -o /tmp/rustup-init "${url}"
echo "$(curl -fsSL --retry 5 "${url}.sha256" | cut -d' ' -f1)  /tmp/rustup-init" | sha256sum -c -
chmod +x /tmp/rustup-init
/tmp/rustup-init -y --no-modify-path --profile minimal --default-toolchain "${RUST_TOOLCHAIN}"
rm -f /tmp/rustup-init
