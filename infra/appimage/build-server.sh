#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Build the server the AppImage's conda package carries (run inside the
# manylinux container scripts/ci/appimage.sh starts, with the checkout at
# /repo and its work directory, holding micromamba, at /work), staging it with
# its web client under /work/server.
#
# This image's glibc, GLIBC_FLOOR, is the AppImage's floor, and the server
# built here links nothing newer. It is built as every native package builds it
# (infra/packaging/server.sh), linking the codec archives built once for the
# architecture, which appimage.sh mounts at SELKIES_CODECS, and it runs against the conda-forge libraries the
# AppImage bundles. libxkbcommon is linked against conda-forge's own, solved
# for the same floor, since this distribution's lacks the API the server reads
# keysyms through. It links nothing but the C library, so it alone is put
# where the linker and the server's own check in server.sh look, and nothing
# else of that prefix stands in for the distribution's.
set -eux
/repo/infra/packaging/portable-deps.sh

XKB=/build/xkbcommon
rm -rf "${XKB}" "${XKB}-link"
env CONDA_OVERRIDE_GLIBC="${GLIBC_FLOOR}" MAMBA_ROOT_PREFIX=/build/mamba \
    /work/conda/micromamba create -y -p "${XKB}" -c conda-forge libxkbcommon
mkdir -p "${XKB}-link"
ln -s "${XKB}/lib/libxkbcommon.so.0" "${XKB}-link/libxkbcommon.so.0"
ln -s libxkbcommon.so.0 "${XKB}-link/libxkbcommon.so"
export LIBRARY_PATH="${XKB}-link" LD_LIBRARY_PATH="${XKB}-link"

PATH="/opt/python/cp312-cp312/bin:${PATH}"
export PATH
rm -rf /work/server
/repo/infra/packaging/server.sh /work/server
chown -R "${HOST_UID:-0}:${HOST_GID:-0}" /work/server
