#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Install the shellcheck this repository lints with.
#
# The shell suite gates on shellcheck's lowest severity, and actionlint runs it
# over every workflow `run:` block. Which findings that severity reports differs
# between releases, so taking whichever build a runner image happens to carry
# turns the tree red on an image bump alone. The version lives here rather than
# in each workflow, because GitHub Actions shares no environment between
# workflow files and a second copy is a second thing to forget.
#
# Usage: install-shellcheck.sh [install-dir]   (default /usr/local/bin)

set -eu

VERSION="0.11.0"

DEST="${1:-/usr/local/bin}"
WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT

case "$(uname -m)" in
    x86_64 | amd64) ARCH=x86_64 ;;
    aarch64 | arm64) ARCH=aarch64 ;;
    *) echo "shellcheck: no release build for $(uname -m)" >&2; exit 1 ;;
esac

TARBALL="shellcheck-v${VERSION}.linux.${ARCH}.tar.xz"
"$(dirname "$0")/fetch.sh" \
    "https://github.com/koalaman/shellcheck/releases/download/v${VERSION}/${TARBALL}" \
    "${WORK}/${TARBALL}"
tar -xJf "${WORK}/${TARBALL}" -C "${WORK}"
# install(1) needs root for a system directory and must not ask for it when the
# destination is already writable, which is how a developer runs this.
if [ -w "${DEST}" ]; then
    install -m 0755 "${WORK}/shellcheck-v${VERSION}/shellcheck" "${DEST}/shellcheck"
else
    sudo install -m 0755 "${WORK}/shellcheck-v${VERSION}/shellcheck" "${DEST}/shellcheck"
fi
"${DEST}/shellcheck" --version
