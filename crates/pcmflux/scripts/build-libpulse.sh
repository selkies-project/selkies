#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Build PulseAudio's client libraries alone (libpulse, libpulse-simple, and the
# libpulsecommon they share), with libsndfile linked into libpulsecommon, for
# the wheel images, whose own libpulse a wheel would carry with what it links:
# CentOS 7's (manylinux2014) is 10.0 and AlmaLinux 8's (manylinux_2_28) 14.0,
# both taking D-Bus and the X11 client libraries along, and Alpine's
# (musllinux) those and libsndfile's codecs. Each
# source is checked against the SHA-256 pinned here (PulseAudio's is the sum its
# release publishes). meson and ninja come from the caller's PATH.
#
# Usage: build-libpulse.sh <prefix> <libdir>   (e.g. /usr lib64)

set -eu

PREFIX="${1:?usage: build-libpulse.sh <prefix> <libdir>}"
LIBDIR="${2:?usage: build-libpulse.sh <prefix> <libdir>}"
SNDFILE=1.2.2
SNDFILE_SHA256=3799ca9924d3125038880367bf1468e53a1b7e3686a934f098b7e1d286cdb80e
PULSE=17.0
PULSE_SHA256=053794d6671a3e397d849e478a80b82a63cb9d8ca296bd35b73317bb5ceb87b5

work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT

# A download a server fails is tried again after a widening wait.
fetch() {
    attempt=1
    until curl -fsSL -o "$3" "$1"; do
        [ "${attempt}" -ge 5 ] && { echo "build-libpulse: no download of $1" >&2; return 1; }
        sleep "$((attempt * 30))"
        attempt="$((attempt + 1))"
    done
    echo "$2  $3" | sha256sum -c -
}

fetch "https://github.com/libsndfile/libsndfile/releases/download/${SNDFILE}/libsndfile-${SNDFILE}.tar.xz" \
    "${SNDFILE_SHA256}" "${work}/sndfile.tar.xz"
fetch "https://freedesktop.org/software/pulseaudio/releases/pulseaudio-${PULSE}.tar.xz" \
    "${PULSE_SHA256}" "${work}/pulse.tar.xz"
mkdir -p "${work}/sndfile-src" "${work}/pulse-src"
tar -xJf "${work}/sndfile.tar.xz" -C "${work}/sndfile-src" --strip-components=1
tar -xJf "${work}/pulse.tar.xz" -C "${work}/pulse-src" --strip-components=1

# libsndfile as a position-independent archive with no codec of its own, which
# only the PulseAudio build below reads; its CMake file predates CMake 3.5.
cmake -S "${work}/sndfile-src" -B "${work}/sndfile-build" -DCMAKE_POLICY_VERSION_MINIMUM=3.5 \
    -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX="${work}/sndfile" -DCMAKE_INSTALL_LIBDIR=lib \
    -DBUILD_SHARED_LIBS=OFF -DCMAKE_POSITION_INDEPENDENT_CODE=ON -DENABLE_EXTERNAL_LIBS=OFF \
    -DENABLE_MPEG=OFF -DBUILD_PROGRAMS=OFF -DBUILD_EXAMPLES=OFF -DBUILD_TESTING=OFF \
    -DENABLE_CPACK=OFF -DINSTALL_MANPAGES=OFF
cmake --build "${work}/sndfile-build" -j"$(nproc)"
cmake --install "${work}/sndfile-build"

# PulseAudio links no libintl where the C library has dgettext, as musl does,
# but GNU gettext's libintl.h, where installed (musllinux), renames the calls
# to libintl's own.
link_args=
if ldd --version 2>&1 | grep -q musl && [ -e /usr/lib/libintl.so ]; then
    link_args=-lintl
fi

PKG_CONFIG_PATH="${work}/sndfile/lib/pkgconfig" meson setup "${work}/pulse-build" "${work}/pulse-src" \
    --prefix="${PREFIX}" --libdir="${LIBDIR}" --buildtype=release -Dc_link_args="${link_args}" \
    -Ddaemon=false -Dclient=true -Ddoxygen=false -Dman=false -Dtests=false -Ddatabase=simple \
    -Dalsa=disabled -Dasyncns=disabled -Davahi=disabled -Dbluez5=disabled -Dconsolekit=disabled \
    -Ddbus=disabled -Delogind=disabled -Dfftw=disabled -Dglib=disabled -Dgstreamer=disabled \
    -Dgtk=disabled -Djack=disabled -Dlirc=disabled -Dopenssl=disabled -Dorc=disabled \
    -Doss-output=disabled -Dsoxr=disabled -Dspeex=disabled -Dsystemd=disabled -Dtcpwrap=disabled \
    -Dudev=disabled -Dvalgrind=disabled -Dwebrtc-aec=disabled -Dx11=disabled
ninja -C "${work}/pulse-build"
ninja -C "${work}/pulse-build" install
