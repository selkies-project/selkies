#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Build the bundle's dbus-daemon into <prefix> from the releases pinned here:
# the message bus a session runs as its session bus and its own system bus
# (crates/selkies/src/commands/session.rs), with expat linked in statically and
# neither systemd, SELinux, the audit library, AppArmor, nor libcap-ng, which a
# session's own buses have no use for, so the daemon needs the C library alone
# and carries nothing of a distribution's. The session bus's configuration,
# session.conf, is this build's too; the system bus's is the bundle's own
# (build-in-image.sh).
#
# Runs in the bundle's build image after infra/bundle/deps.sh, once into the
# toolchain image (infra/bundle/Dockerfile); a <prefix> holding a finished build
# is left as it is.
#
# Usage: dbus.sh <prefix>
#   writes <prefix>/bin/dbus-daemon and <prefix>/share/dbus-1/session.conf
set -eux
PREFIX="$(mkdir -p "${1:?usage: dbus.sh <prefix>}" && readlink -f "$1")"
[ ! -f "${PREFIX}/.complete" ] || exit 0
DBUS_VERSION="1.16.4"
DBUS_SHA256="d08b05526c2cb7966a5fb79210d6112b1aca3d135f315c1b302e14b4b0297dac"
EXPAT_VERSION="2.10.0"
EXPAT_SHA256="c624ba2998fc65df1163a49641ff3f9ab73138b11641d92c86bee0fb01b1bc7f"
WORK="$(mktemp -d)"
export PATH="/opt/python/cp312-cp312/bin:${PATH}"

# fetch <url> <sha256> <dir>: the archive checked and unpacked into <dir>
fetch() {
    curl -fsSL --retry 8 --retry-delay 30 --connect-timeout 30 -o "${WORK}/archive.tar.xz" "$1"
    echo "$2  ${WORK}/archive.tar.xz" | sha256sum -c -
    mkdir -p "$3"
    tar -xf "${WORK}/archive.tar.xz" -C "$3" --strip-components=1
}

fetch "https://github.com/libexpat/libexpat/releases/download/R_$(echo "${EXPAT_VERSION}" | tr . _)/expat-${EXPAT_VERSION}.tar.xz" \
    "${EXPAT_SHA256}" "${WORK}/expat"
(cd "${WORK}/expat" && ./configure --prefix="${WORK}/static" --libdir="${WORK}/static/lib" \
    --disable-shared --enable-static --with-pic --without-docbook --without-examples \
    --without-tests > /dev/null && make -j"$(nproc)" > /dev/null && make install > /dev/null)

fetch "https://dbus.freedesktop.org/releases/dbus/dbus-${DBUS_VERSION}.tar.xz" "${DBUS_SHA256}" \
    "${WORK}/dbus"
PKG_CONFIG_PATH="${WORK}/static/lib/pkgconfig${PKG_CONFIG_PATH:+:${PKG_CONFIG_PATH}}" \
    meson setup "${WORK}/dbus/build" "${WORK}/dbus" --prefix=/usr --sysconfdir=/etc \
    --localstatedir=/var --buildtype=release --wrap-mode=nodownload --default-library=static \
    -Dprefer_static=true \
    -Dmessage_bus=true -Dsystemd=disabled -Dselinux=disabled -Dlibaudit=disabled \
    -Dapparmor=disabled -Dx11_autolaunch=disabled -Dkqueue=disabled -Dlaunchd=disabled \
    -Duser_session=false -Dtools=false -Dmodular_tests=disabled -Dinstalled_tests=false \
    -Ddoxygen_docs=disabled -Dducktype_docs=disabled -Dxml_docs=disabled -Dqt_help=disabled
ninja -C "${WORK}/dbus/build" bus/dbus-daemon
install -Dm755 "${WORK}/dbus/build/bus/dbus-daemon" "${PREFIX}/bin/dbus-daemon"
install -Dm644 "${WORK}/dbus/build/bus/session.conf" "${PREFIX}/share/dbus-1/session.conf"

# Nothing but the C library
other="$(readelf -d "${PREFIX}/bin/dbus-daemon" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p' \
    | grep -vxE 'lib(c|m|dl|pthread|rt)\.so\.[0-9]+|ld-linux[-a-z0-9_]*\.so\.[0-9]+|(ld|libc)\.musl-[a-z0-9_]+\.so\.1' || true)"
[ -z "${other}" ] || { echo "dbus-daemon links ${other}" >&2; exit 1; }
"${PREFIX}/bin/dbus-daemon" --version | grep -F "${DBUS_VERSION}"
rm -rf "${WORK}"
touch "${PREFIX}/.complete"
