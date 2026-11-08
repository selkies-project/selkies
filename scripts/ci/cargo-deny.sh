#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Run `cargo deny check <check...>` with cargo-deny's latest release: its
# binary from GitHub, fetched with retries and checked against the SHA-256
# published beside it, or, where GitHub's release downloads stay down past the
# retries, built from crates.io, whose checksums cargo checks. The crate index
# and the advisory database are fetched under retries too, and the check then
# runs without fetching.
#
# Usage: cargo-deny.sh <check...>   (from the repository root)

set -eu

HERE="$(dirname "$(readlink -f "$0")")"
work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT

release() {
    # The latest release's tag, from where GitHub redirects /releases/latest to
    latest="$(curl -fsSL --retry 3 -o /dev/null -w '%{url_effective}' \
        https://github.com/EmbarkStudios/cargo-deny/releases/latest)" || return 1
    version="${latest##*/}"
    case "${version}" in
        '' | *[!0-9.]*) echo "cargo-deny: no release tag in ${latest}" >&2; return 1 ;;
    esac
    asset="cargo-deny-${version}-$(uname -m)-unknown-linux-musl.tar.gz"
    base="https://github.com/EmbarkStudios/cargo-deny/releases/download/${version}"
    FETCH_ATTEMPTS=4 "${HERE}/fetch.sh" "${base}/${asset}.sha256" "${work}/sum" || return 1
    FETCH_ATTEMPTS=4 "${HERE}/fetch.sh" "${base}/${asset}" "${work}/deny.tar.gz" \
        "$(cut -d' ' -f1 "${work}/sum")" || return 1
    tar -xzf "${work}/deny.tar.gz" -C "${work}" --strip-components=1
}

if ! release; then
    echo "cargo-deny: GitHub's release download failed; building it from crates.io" >&2
    "${HERE}/retry.sh" 5 60 cargo install --locked --quiet --root "${work}/root" cargo-deny
    mv "${work}/root/bin/cargo-deny" "${work}/cargo-deny"
fi
"${work}/cargo-deny" --version

"${HERE}/retry.sh" 5 60 "${work}/cargo-deny" fetch all
"${work}/cargo-deny" --offline check "$@"
