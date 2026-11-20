#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Build alsa-plugins' PulseAudio plugin alone into <prefix>, from the releases
# pinned here: the PCM and control modules a session's ALSA-only applications
# reach the session's sound server through, which `selkies session` names to
# them (crates/selkies/src/commands/session.rs, `Session::alsa_bridge`). A
# distribution's libasound2-plugins carries the same two modules among plugins
# that take FFmpeg, JACK, libsamplerate, and Speex along; here every other
# plugin is left out, so the modules link libasound and libpulse alone.
#
# The modules are loaded by the host's libasound into the application's own
# process, so alsa-lib is built here only to be linked against and is never
# carried. Its plugin interfaces (ioplug 1.0.2, ctl_ext 1.0.1) are those of
# every alsa-lib since 1.0.25, and every symbol the modules take from it is of
# the ALSA_0.9 versions, which the build checks: alsa-lib 1.2.15 turned its
# error macro into a call of a symbol it added (snd_lib_log), so modules built
# against it or later would load in no older libasound, which is why the
# release pinned here is 1.2.14. They link the libpulse build-libpulse.sh
# installed into the image, the one the bundle carries.
#
# Runs in the bundle's build image after infra/bundle/deps.sh, once into the
# toolchain image (infra/bundle/Dockerfile); a <prefix> holding a finished build
# is left as it is.
#
# Usage: alsa-pulse.sh <prefix>
#   writes <prefix>/lib/libasound_module_pcm_pulse.so and libasound_module_ctl_pulse.so
set -eux
PREFIX="$(mkdir -p "${1:?usage: alsa-pulse.sh <prefix>}" && readlink -f "$1")"
[ ! -f "${PREFIX}/.complete" ] || exit 0
ALSA_LIB_VERSION="1.2.14"
ALSA_LIB_SHA256="be9c88a0b3604367dd74167a2b754a35e142f670292ae47a2fdef27a2ee97a32"
ALSA_PLUGINS_VERSION="1.2.12"
ALSA_PLUGINS_SHA256="7bd8a83d304e8e2d86a25895d8dcb0ef0245a8df32e271959cdbdc6af39b66f2"
WORK="$(mktemp -d)"

# fetch <url> <sha256> <dir>: the archive checked and unpacked into <dir>
fetch() {
    curl -fsSL --retry 8 --retry-delay 30 --connect-timeout 30 -o "${WORK}/archive.tar.bz2" "$1"
    echo "$2  ${WORK}/archive.tar.bz2" | sha256sum -c -
    mkdir -p "$3"
    tar -xjf "${WORK}/archive.tar.bz2" -C "$3" --strip-components=1
}

fetch "https://www.alsa-project.org/files/pub/lib/alsa-lib-${ALSA_LIB_VERSION}.tar.bz2" \
    "${ALSA_LIB_SHA256}" "${WORK}/alsa-lib"
(cd "${WORK}/alsa-lib" && ./configure --prefix="${WORK}/alsa" --disable-static \
    --disable-python --disable-topology --disable-ucm --without-debug > /dev/null \
    && make -j"$(nproc)" > /dev/null && make install > /dev/null)

fetch "https://www.alsa-project.org/files/pub/plugins/alsa-plugins-${ALSA_PLUGINS_VERSION}.tar.bz2" \
    "${ALSA_PLUGINS_SHA256}" "${WORK}/alsa-plugins"
(cd "${WORK}/alsa-plugins" \
    && PKG_CONFIG_PATH="${WORK}/alsa/lib/pkgconfig${PKG_CONFIG_PATH:+:${PKG_CONFIG_PATH}}" \
        CPPFLAGS="-I${WORK}/alsa/include" LDFLAGS="-L${WORK}/alsa/lib" ./configure \
        --prefix="${WORK}/plugins" --disable-static --disable-oss --disable-mix \
        --disable-usbstream --disable-arcamav --disable-jack --disable-samplerate \
        --disable-libav --disable-a52 --disable-lavrate --disable-speexdsp --disable-aaf \
        --disable-maemo-plugin --with-speex=no > /dev/null \
    && make -C pulse -j"$(nproc)" > /dev/null)
mkdir -p "${PREFIX}/lib"
for module in pcm ctl; do
    install -m755 "${WORK}/alsa-plugins/pulse/.libs/libasound_module_${module}_pulse.so" \
        "${PREFIX}/lib/"
    patchelf --remove-rpath "${PREFIX}/lib/libasound_module_${module}_pulse.so"
    strip --strip-unneeded "${PREFIX}/lib/libasound_module_${module}_pulse.so"
done

# libasound and libpulse alone, beside the C library, and nothing of libasound
# newer than its ALSA_0.9 symbols
other="$(readelf -d "${PREFIX}"/lib/*.so | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p' | sort -u \
    | grep -vE '^(libasound\.so\.2|libpulse\.so\.0|lib(c|m|dl|pthread|rt)\.so\.[0-9]+|libc\.musl.*|ld-.*)$' \
    || true)"
[ -z "${other}" ] || { echo "alsa-pulse: the modules link ${other}" >&2; exit 1; }
newer="$(objdump -T "${PREFIX}"/lib/*.so | grep -oE 'ALSA_[0-9a-z.]+' | grep -vE '^ALSA_0\.9' || true)"
[ -z "${newer}" ] || { echo "alsa-pulse: the modules take ${newer} of libasound" >&2; exit 1; }
rm -rf "${WORK}"
touch "${PREFIX}/.complete"
