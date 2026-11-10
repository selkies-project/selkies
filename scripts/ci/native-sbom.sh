#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Describe the native codec libraries the server links, one CycloneDX SBOM per
# pixelflux build: the GPL build (the default) and the non-GPL one
# (PIXELFLUX_ENABLE_GPL=0). They are the libraries the wheel recipe builds
# (crates/pixelflux/pyproject.toml, build_codec_libraries) at the commits it
# pins, which codec-sys binds, and the copies crates compile from the sources
# they bundle: libjpeg-turbo (turbojpeg-sys) and Opus (opusic-sys) in both, and
# OpenH264 (openh264-sys2) in the non-GPL build. Each is a pkg:generic purl at its
# release, the form Snyk's database matches C libraries by; x264 is pinned to a
# commit, which stands as its version.
#
# Usage: native-sbom.sh <directory>
#   writes <directory>/native-gpl.cdx.json and <directory>/native-non-gpl.cdx.json
set -eu
OUT="${1:?usage: native-sbom.sh <directory>}"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
RECIPE="${ROOT}/crates/pixelflux/pyproject.toml"

# The recipe's build_codec_libraries: what both builds build, and what each adds
BOTH="libvpx svtav1 dav1d libde265"
GPL_ONLY="x264 x265"
NON_GPL_ONLY="kvazaar"

# "name commit tag" for every library the recipe clones at a pinned commit
PINS="$(awk '$1 == "clone_source" && length($2) == 40 { print $4, $2, $3 }' "${RECIPE}")"
[ -n "${PINS}" ] || { echo "native-sbom: no pinned clone_source in ${RECIPE}" >&2; exit 1; }
for name in $(printf '%s\n' "${PINS}" | cut -d' ' -f1); do
    case " ${BOTH} ${GPL_ONLY} ${NON_GPL_ONLY} " in
        *" ${name} "*) ;;
        *) echo "native-sbom: ${name} is pinned but in no build's list" >&2; exit 1 ;;
    esac
done

# The releases the crates bundle, read from their sources
METADATA="$(cargo metadata --format-version 1 --locked --all-features --manifest-path "${ROOT}/Cargo.toml")"
crate_dir() {
    printf '%s\n' "${METADATA}" \
        | jq -r --arg name "$1" 'first(.packages[] | select(.name == $name) | .manifest_path) // "/missing/crate"' \
        | xargs dirname
}
# codec_ver.h has CRLF lines
OPENH264="$(tr -d '\r' < "$(crate_dir openh264-sys2)/upstream/codec/api/wels/codec_ver.h" \
    | awk '$1 == "#define" && $2 ~ /^OPENH264_(MAJOR|MINOR|REVISION)$/ {
        gsub(/[()]/, "", $3); version = version (version == "" ? "" : ".") $3
    } END { print version }')"
LIBJPEG_TURBO="$(awk '$1 == "set(VERSION" { sub(/\)$/, "", $2); print $2; exit }' \
    "$(crate_dir turbojpeg-sys)/libjpeg-turbo/CMakeLists.txt")"
OPUS="$(sed -n 's/^PACKAGE_VERSION="\(.*\)"$/\1/p' "$(crate_dir opusic-sys)/opus/package_version")"
for version in "openh264-sys2 ${OPENH264}" "turbojpeg-sys ${LIBJPEG_TURBO}" "opusic-sys ${OPUS}"; do
    case "${version#* }" in
        [0-9]*.[0-9]*) ;;
        *) echo "native-sbom: no release read from ${version% *}'s bundled source" >&2; exit 1 ;;
    esac
done

# One component: the name Snyk knows the library by, its release (the tag without
# a leading v, or the commit where the recipe pins no tag), and the commit
component() {
    pin="$(printf '%s\n' "${PINS}" | awk -v name="$1" '$1 == name')"
    [ -n "${pin}" ] || { echo "native-sbom: $1 is not pinned in ${RECIPE}" >&2; exit 1; }
    commit="$(echo "${pin}" | cut -d' ' -f2)"
    tag="$(echo "${pin}" | cut -d' ' -f3)"
    version="${tag#v}"
    [ "${tag}" != "-" ] || version="${commit}"
    case "$1" in
        svtav1) purl_name="svt-av1" ;;
        *) purl_name="$1" ;;
    esac
    jq -n --arg name "${purl_name}" --arg version "${version}" --arg commit "${commit}" \
        '{type: "library", "bom-ref": $name, name: $name, version: $version,
          purl: "pkg:generic/\($name)@\($version)",
          properties: [{name: "commit", value: $commit}]}'
}

# One component a crate bundles: its name, release, and the crate
bundled() {
    jq -n --arg name "$1" --arg version "$2" --arg crate "$3" \
        '{type: "library", "bom-ref": $name, name: $name, version: $version,
          purl: "pkg:generic/\($name)@\($version)",
          properties: [{name: "bundled-by", value: $crate}]}'
}

# One build's SBOM from the file of its components, a JSON stream
sbom() {
    jq -s --arg build "$1" \
        '{bomFormat: "CycloneDX", specVersion: "1.5", version: 1,
          metadata: {component: {type: "library", "bom-ref": "selkies",
                                 name: "Selkies codec libraries (\($build) build)"}},
          components: .,
          dependencies: [{ref: "selkies", dependsOn: [.[]["bom-ref"]]}]}' "$2"
}

TMP="$(mktemp -d)"
trap 'rm -rf "${TMP}"' EXIT
for build in gpl non-gpl; do
    {
        bundled libjpeg-turbo "${LIBJPEG_TURBO}" turbojpeg-sys
        bundled opus "${OPUS}" opusic-sys
    } > "${TMP}/${build}"
done
for name in ${BOTH} ${GPL_ONLY}; do component "${name}"; done >> "${TMP}/gpl"
for name in ${BOTH} ${NON_GPL_ONLY}; do component "${name}"; done >> "${TMP}/non-gpl"
bundled openh264 "${OPENH264}" openh264-sys2 >> "${TMP}/non-gpl"
mkdir -p "${OUT}"
sbom GPL "${TMP}/gpl" > "${OUT}/native-gpl.cdx.json"
sbom non-GPL "${TMP}/non-gpl" > "${OUT}/native-non-gpl.cdx.json"
