#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Run `cargo deny check <check...>` with cargo-deny's latest release, fetched
# with retries and checked against the SHA-256 published beside it. The crate
# index and the advisory database are fetched under retries too, and the check
# then runs without fetching, so an outage of GitHub's release or git hosting
# fails a run only when it outlasts every attempt.
#
# Usage: cargo-deny.sh <check...>   (from the repository root)

set -eu

HERE="$(dirname "$(readlink -f "$0")")"
work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT

# The latest release's tag, from where GitHub redirects /releases/latest to
latest="$("${HERE}/retry.sh" 4 30 curl -fsSL -o /dev/null -w '%{url_effective}' \
    https://github.com/EmbarkStudios/cargo-deny/releases/latest)"
version="${latest##*/}"
case "${version}" in
    '' | *[!0-9.]*) echo "cargo-deny: no release tag in ${latest}" >&2; exit 1 ;;
esac
asset="cargo-deny-${version}-$(uname -m)-unknown-linux-musl.tar.gz"
base="https://github.com/EmbarkStudios/cargo-deny/releases/download/${version}"
"${HERE}/fetch.sh" "${base}/${asset}.sha256" "${work}/sum"
"${HERE}/fetch.sh" "${base}/${asset}" "${work}/deny.tar.gz" "$(cut -d' ' -f1 "${work}/sum")"
tar -xzf "${work}/deny.tar.gz" -C "${work}" --strip-components=1
"${work}/cargo-deny" --version

"${HERE}/retry.sh" 4 30 "${work}/cargo-deny" fetch all
"${work}/cargo-deny" --offline check "$@"
