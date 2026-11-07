#!/bin/bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# linuxdeploy plugin that bundles a conda environment into an AppDir, in the
# same shape as https://github.com/linuxdeploy/linuxdeploy-plugin-conda and
# with the same variables, but creating the prefix with micromamba instead of
# installing Miniconda3: micromamba is one static executable (BSD-3-Clause)
# that solves and installs without a Python interpreter, so the prefix holds
# the packages asked for and none of conda's own, and it resolves against the
# channels named alone, so nothing here is subject to the Anaconda repository
# terms. That upstream plugin also publishes no release artifacts, so it is
# vendored rather than downloaded.
#
# Variables:
#   CONDA_CHANNELS="channelA;channelB;..."   (in priority order)
#   CONDA_PACKAGES="packageA;packageB;..."
#   ARCH="x86_64" (also aarch64)
set -e
[ -n "${DEBUG:-}" ] && set -x

ARCH="${ARCH:-$(uname -m)}"
APPDIR=

while [ "$#" -gt 0 ]; do
    case "$1" in
        --plugin-api-version) echo "0"; exit 0 ;;
        --appdir) APPDIR="$(readlink -f "$2")"; shift 2 ;;
        --help) sed -n '2,19p' "$0"; exit 0 ;;
        *) echo "invalid argument: $1" >&2; exit 1 ;;
    esac
done
[ -n "${APPDIR}" ] || { echo "usage: $0 --appdir <path>" >&2; exit 1; }

DOWNLOAD_DIR="${CONDA_DOWNLOAD_DIR:-/tmp/linuxdeploy-plugin-conda-$(id -u)}"
# micromamba maps CONDA_*-named environment variables onto its own
# configuration, and `channels` is one of them: left in the environment, the
# solver reads the whole ';'-separated list as a single channel name. Take a
# copy and clear them before anything is invoked.
CHANNELS="${CONDA_CHANNELS:-}"
PACKAGES="${CONDA_PACKAGES:-}"
unset CONDA_CHANNELS CONDA_PACKAGES

# Retry every remote fetch below, with mamba's own backoff factor of 3 matched
# rather than lowered. Unrelated to the CONDA_* names cleared above, which are
# this plugin's inputs that micromamba would misread as its config.
export MAMBA_REMOTE_MAX_RETRIES="5" MAMBA_REMOTE_BACKOFF_FACTOR="3" \
    MAMBA_REMOTE_CONNECT_TIMEOUT_SECS="30"
# Its package cache stays beside the download rather than in the prefix
export MAMBA_ROOT_PREFIX="${DOWNLOAD_DIR}/root"

mkdir -p "${DOWNLOAD_DIR}" "${APPDIR}"
# micromamba names its executables after conda's platform, which is `uname -m`
# for every architecture but x86_64
case "${ARCH}" in
    x86_64) ASSET="micromamba-linux-64" ;;
    *) ASSET="micromamba-linux-${ARCH}" ;;
esac
MAMBA="${DOWNLOAD_DIR}/micromamba"
# The latest release, checked against the SHA-256 recorded for it.
# fetch-verified reads that off GitHub's API and waits out the rate limit it
# answers with; it sits beside this plugin when scripts/ci/appimage.sh placed it
# there, and a direct run of the plugin takes the latest release's executable
# and the `.sha256` micromamba publishes beside it.
HERE="$(dirname "$(readlink -f "$0")")"
if [ -x "${HERE}/fetch-verified" ]; then
    "${HERE}/fetch-verified" release mamba-org/micromamba-releases latest "${ASSET}" "${MAMBA}"
else
    CURL=(curl -fsSL --retry 5 --retry-delay 3 --retry-connrefused --retry-max-time 180)
    URL="https://github.com/mamba-org/micromamba-releases/releases/latest/download/${ASSET}"
    "${CURL[@]}" -o "${MAMBA}" "${URL}"
    SUM="$("${CURL[@]}" "${URL}.sha256" | cut -d' ' -f1)"
    if [ "$(sha256sum "${MAMBA}" | cut -d' ' -f1)" != "${SUM}" ]; then
        echo "micromamba: ${ASSET} does not match its published SHA-256 ${SUM}" >&2
        rm -f "${MAMBA}"
        exit 1
    fi
fi
chmod +x "${MAMBA}"

# usr/conda rather than usr/ so these libraries cannot collide with what
# linuxdeploy and its other plugins bundle
PREFIX="${APPDIR}/usr/conda"

# Keep micromamba away from the invoking user's configuration
mkdir -p "${APPDIR}/.conda-home"
export HOME
HOME="$(readlink -f "${APPDIR}/.conda-home")"

channel_args=()
IFS=';' read -ra chans <<< "${CHANNELS}"
for chan in "${chans[@]}"; do
    [ -n "${chan}" ] && channel_args+=(-c "${chan}")
done

pkgs=()
IFS=';' read -ra requested <<< "${PACKAGES}"
for pkg in "${requested[@]}"; do
    [ -n "${pkg}" ] && pkgs+=("${pkg}")
done
if [ "${#pkgs[@]}" -eq 0 ]; then
    echo "WARNING: no CONDA_PACKAGES requested" >&2
else
    # One transaction so the solver sees the whole set. Copies rather than
    # links into its package cache, which the cleanup below would strip
    # through the links.
    "${MAMBA}" create -y -p "${PREFIX}" --always-copy "${channel_args[@]}" "${pkgs[@]}"
fi

# linuxdeploy looks for the executables in usr/bin
mkdir -p "${APPDIR}/usr/bin"
for path in "${PREFIX}"/bin/*; do
    name="$(basename "${path}")"
    [ -e "${APPDIR}/usr/bin/${name}" ] || ln -s "../conda/bin/${name}" "${APPDIR}/usr/bin/${name}"
done

# Drop what an AppImage never needs; this is most of the payload
if [ "${CONDA_SKIP_CLEANUP:-}" != "1" ]; then
    rm -rf "${PREFIX}/pkgs" "${PREFIX}/lib/cmake" "${PREFIX}/share/doc" \
           "${PREFIX}/share/gtk-doc" "${PREFIX}/share/man" "${APPDIR}/.conda-home"
    find "${PREFIX}" -type f -name '*.a' -delete
    find "${PREFIX}" -type f -name '*.so*' -exec strip --strip-unneeded {} + 2>/dev/null || true
fi
