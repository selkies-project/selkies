#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Side-load the OpenH264 GMP plugin into the e2e Firefox profile.
#
# Playwright's Firefox ships no OpenH264, and without it Firefox answers a
# WebRTC offer with the video m-line rejected, so the whole firefox-wr block
# fails on one absence. Firefox would fetch the plugin itself on first use, but
# that is a background download the suite cannot wait on, so it is placed here
# instead: the plugin is taken from the same update service Firefox itself
# queries, and verified against the checksum that service publishes.
#
# Writes $E2E_FIREFOX_PROFILE/gmp-gmpopenh264/<version>/, which is where
# Firefox looks once media.gmp-gmpopenh264.version names that version (set in
# tests/e2e/test_browsers.py alongside the other profile prefs).
set -eu

# Defaults track tests/helpers.py's WORKDIR, which is what test_browsers.py
# resolves the profile against; a different fallback here installs the plugin
# where the suite does not look and the firefox-wr block skips.
PROFILE="${E2E_FIREFOX_PROFILE:-${E2E_WORKDIR:-${TMPDIR:-/tmp}/selkies-tests}/firefox-profile}"

case "$(uname -m)" in
    x86_64) TARGET=Linux_x86_64-gcc3 ;;
    aarch64|arm64) TARGET=Linux_aarch64-gcc3 ;;
    *) echo "openh264: no GMP build for $(uname -m)" >&2; exit 1 ;;
esac

XML="https://aus5.mozilla.org/update/3/GMP/128.0/20240101000000/${TARGET}/en-US/release/default/default/default/update.xml"
ENTRY="$(curl -fsSL "${XML}" | tr '>' '\n' | grep 'id="gmp-gmpopenh264"')"

attr() { printf '%s' "${ENTRY}" | sed -n "s/.*[[:space:]]$1=\"\([^\"]*\)\".*/\1/p"; }
URL="$(attr URL)"
VERSION="$(attr version)"
WANT="$(attr hashValue)"
if [ -z "${URL}" ] || [ -z "${VERSION}" ] || [ -z "${WANT}" ]; then
    echo "openh264: update service returned no usable entry" >&2
    exit 1
fi

DEST="${PROFILE}/gmp-gmpopenh264/${VERSION}"
if [ -f "${DEST}/libgmpopenh264.so" ]; then
    echo "openh264: ${VERSION} already present in ${DEST}"
    exit 0
fi

TMP="$(mktemp -d)"
trap 'rm -rf "${TMP}"' EXIT
curl -fsSL -o "${TMP}/gmp.zip" "${URL}"

GOT="$(sha512sum "${TMP}/gmp.zip" | cut -d' ' -f1)"
[ "${GOT}" = "${WANT}" ] || {
    echo "openh264: checksum mismatch (want ${WANT}, got ${GOT})" >&2; exit 1; }

mkdir -p "${DEST}"
unzip -oq "${TMP}/gmp.zip" -d "${DEST}"
test -f "${DEST}/libgmpopenh264.so"
echo "openh264: installed ${VERSION} into ${DEST}"
