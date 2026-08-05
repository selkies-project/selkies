#!/bin/bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# linuxdeploy plugin that bundles a conda environment into an AppDir, in the
# same shape as https://github.com/linuxdeploy/linuxdeploy-plugin-conda and
# with the same variables, but installing Miniforge instead of Miniconda3:
# Miniforge is BSD-3-Clause and resolves against conda-forge alone, so nothing
# here is subject to the Anaconda repository terms. That upstream plugin also
# publishes no release artifacts, so it is vendored rather than downloaded.
#
# Variables:
#   CONDA_CHANNELS="channelA;channelB;..."   (in priority order)
#   CONDA_PACKAGES="packageA;packageB;..."
#   CONDA_PYTHON_VERSION="3.12"
#   PIP_REQUIREMENTS="packageA packageB -r requirements.txt"
#   ARCH="x86_64" (also aarch64)
set -e
[ -n "${DEBUG:-}" ] && set -x

ARCH="${ARCH:-$(uname -m)}"
APPDIR=

while [ "$#" -gt 0 ]; do
    case "$1" in
        --plugin-api-version) echo "0"; exit 0 ;;
        --appdir) APPDIR="$(readlink -f "$2")"; shift 2 ;;
        --help) sed -n '2,22p' "$0"; exit 0 ;;
        *) echo "invalid argument: $1" >&2; exit 1 ;;
    esac
done
[ -n "${APPDIR}" ] || { echo "usage: $0 --appdir <path>" >&2; exit 1; }

DOWNLOAD_DIR="${CONDA_DOWNLOAD_DIR:-/tmp/linuxdeploy-plugin-conda-$(id -u)}"
# conda maps CONDA_*-named environment variables onto its own configuration, and
# `channels` is one of them: left in the environment, the solver reads the whole
# ';'-separated list as a single channel name. pip does the same with PIP_*. Take
# a copy and clear them before anything is invoked.
CHANNELS="${CONDA_CHANNELS:-}"
PACKAGES="${CONDA_PACKAGES:-}"
PYTHON_VERSION="${CONDA_PYTHON_VERSION:-}"
REQUIREMENTS="${PIP_REQUIREMENTS:-}"
unset CONDA_CHANNELS CONDA_PACKAGES CONDA_PYTHON_VERSION PIP_REQUIREMENTS

mkdir -p "${DOWNLOAD_DIR}" "${APPDIR}"
# Miniforge names its installers after `uname -m`
INSTALLER="Miniforge3-Linux-${ARCH}.sh"
# GitHub rate-limits release downloads. fetch.sh waits out the Retry-After it
# sends; it sits beside this plugin when scripts/ci/appimage.sh placed it there,
# and a direct run of the plugin falls back to a plain download.
HERE="$(dirname "$(readlink -f "$0")")"
if [ -x "${HERE}/fetch.sh" ]; then
    "${HERE}/fetch.sh" \
        "https://github.com/conda-forge/miniforge/releases/latest/download/${INSTALLER}" \
        "${DOWNLOAD_DIR}/${INSTALLER}"
else
    curl -fsSL -o "${DOWNLOAD_DIR}/${INSTALLER}" \
        "https://github.com/conda-forge/miniforge/releases/latest/download/${INSTALLER}"
fi

# usr/conda rather than usr/ so these libraries cannot collide with what
# linuxdeploy and its other plugins bundle
PREFIX="${APPDIR}/usr/conda"
bash "${DOWNLOAD_DIR}/${INSTALLER}" -b -p "${PREFIX}" -f

# Keep conda away from the invoking user's configuration
mkdir -p "${APPDIR}/.conda-home"
export HOME
HOME="$(readlink -f "${APPDIR}/.conda-home")"

channel_args=()
IFS=';' read -ra chans <<< "${CHANNELS}"
for chan in "${chans[@]}"; do
    [ -n "${chan}" ] && channel_args+=(-c "${chan}")
done

pkgs=()
[ -n "${PYTHON_VERSION}" ] && pkgs+=("python=${PYTHON_VERSION}")
IFS=';' read -ra requested <<< "${PACKAGES}"
for pkg in "${requested[@]}"; do
    [ -n "${pkg}" ] && pkgs+=("${pkg}")
done
if [ "${#pkgs[@]}" -eq 0 ]; then
    echo "WARNING: no CONDA_PACKAGES requested" >&2
else
    # One transaction so the solver sees the whole set; mamba ships with
    # Miniforge and conda is the fallback
    SOLVER="${PREFIX}/bin/mamba"
    [ -x "${SOLVER}" ] || SOLVER="${PREFIX}/bin/conda"
    "${SOLVER}" install -y "${channel_args[@]}" "${pkgs[@]}"
fi

if [ -n "${REQUIREMENTS}" ]; then
    # shellcheck disable=SC2086  # the requirement list is intentionally split
    "${PREFIX}/bin/pip" install -U ${REQUIREMENTS}
fi

# linuxdeploy looks for the entry points in usr/bin
mkdir -p "${APPDIR}/usr/bin"
for path in "${PREFIX}"/bin/*; do
    name="$(basename "${path}")"
    [ -e "${APPDIR}/usr/bin/${name}" ] || ln -s "../conda/bin/${name}" "${APPDIR}/usr/bin/${name}"
done

# Drop what an AppImage never needs; this is most of the payload
if [ "${CONDA_SKIP_CLEANUP:-}" != "1" ]; then
    rm -rf "${PREFIX}/pkgs" "${PREFIX}/lib/cmake" "${PREFIX}/share/doc" \
           "${PREFIX}/share/gtk-doc" "${PREFIX}/share/man" "${APPDIR}/.conda-home"
    find "${PREFIX}" -type d -name '__pycache__' -prune -exec rm -rf {} +
    find "${PREFIX}" -type f -name '*.a' -delete
    find "${PREFIX}" -type f -name '*.so*' -exec strip --strip-unneeded {} + 2>/dev/null || true
fi
