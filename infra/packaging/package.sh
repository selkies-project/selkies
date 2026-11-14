#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Repackage a Selkies bundle (infra/bundle/build.sh) as a native package of
# <format>, run inside a container of a distribution that format's own tool
# comes from: deb (Debian or Ubuntu, dpkg-deb), rpm (Fedora or Enterprise
# Linux, rpmbuild), apk (Alpine, abuild), or arch (Arch Linux, makepkg). Nothing
# is compiled: every package carries the same tree the bundle does, unpacked at
# /opt/selkies, with its programs linked onto /usr/bin. So one package of a
# format installs on every release of every distribution that takes the
# format, down to the bundle's C library floor, and runs the same binaries as
# every other channel.
#
# The bundle needs nothing of a host but its C library, the C++ runtime on
# glibc, and the graphics stack, which it loads where present and goes without
# otherwise: those are what each package declares, the graphics stack as a weak
# dependency. The interposers are also linked under the library directories the
# loader's `$LIB` names on that distribution, so `/usr/$LIB/selkies_input_interposer.so`
# preloads the build of each process's word size, the 32-bit one where the
# bundle carries it.
#
# Usage: package.sh <deb|rpm|apk|arch> <bundle.tar.gz> <out-dir>
#   (the version is the one the bundle's name carries)
set -eux
FORMAT="${1:?usage: package.sh <deb|rpm|apk|arch> <bundle.tar.gz> <out-dir>}"
BUNDLE="$(readlink -f "${2:?usage: package.sh <format> <bundle.tar.gz> <out-dir>}")"
OUT="$(mkdir -p "${3:?usage: package.sh <format> <bundle.tar.gz> <out-dir>}" && readlink -f "$3")"
REPO="$(dirname "$(readlink -f "$0")")/../.."
NAME="$(basename "${BUNDLE}" .tar.gz)"
case "${NAME}" in
    selkies-*-linux-gnu|selkies-*-linux-musl) ;;
    *) echo "package.sh: ${NAME} is not a bundle" >&2; exit 2 ;;
esac
ARCH="${NAME%-linux-*}"
ARCH="${ARCH##*-}"
VERSION="${NAME#selkies-}"
VERSION="${VERSION%-"${ARCH}"-linux-*}"
[ "${ARCH}" = "$(uname -m)" ] || { echo "package.sh: a ${ARCH} bundle on $(uname -m)" >&2; exit 2; }
# The glibc a glibc bundle needs, its builder image's (infra/bundle/toolchain.sh).
# Enterprise Linux 7's rpm (4.11) reads no zstd payload, the default of the
# Fedora rpmbuild that packs it: a bundle that runs there, below Enterprise
# Linux 8's glibc 2.28, is packed with xz.
case "${NAME}" in
    *-linux-gnu)
        platform="$(sh "${REPO}/infra/bundle/toolchain.sh" platform glibc "${ARCH}")"
        GLIBC="$(echo "${platform#manylinux_}" | tr _ .)"
        RPM_PAYLOAD=""
        if [ "$(printf '%s\n2.28\n' "${GLIBC}" | sort -V | head -n 1)" != 2.28 ]; then
            RPM_PAYLOAD="%global _binary_payload w6T.xzdio"
        fi
        ;;
esac
WORK="${SELKIES_BUILD_DIR:-/build}/package"
ROOT="${WORK}/root"
SUMMARY="Low-latency HTML5 remote desktop streaming (WebSocket and WebRTC)"
URL="https://github.com/selkies-project/selkies"
# shellcheck source=infra/packaging/version.sh
. "${REPO}/infra/packaging/version.sh"

# Package managers with no retry option of their own are bounded-retried
retry() {
    i=1
    until "$@"; do
        [ "${i}" -ge 5 ] && return 1
        i=$((i + 1)); sleep 5
    done
}

rm -rf "${WORK}"
mkdir -p "${ROOT}/opt/selkies" "${ROOT}/usr/bin"
tar -xzf "${BUNDLE}" -C "${ROOT}/opt/selkies" --strip-components=1
test -x "${ROOT}/opt/selkies/bin/selkies"
for program in selkies selkies-desktop; do
    ln -s "/opt/selkies/bin/${program}" "${ROOT}/usr/bin/${program}"
done

# `$LIB` for each word size, as this distribution's loader expands it
case "${FORMAT}" in
    deb)
        multiarch="$(dpkg-architecture -qDEB_HOST_MULTIARCH 2> /dev/null \
            || echo "${ARCH}-linux-gnu")"
        lib64="usr/lib/${multiarch}" lib32="usr/lib/i386-linux-gnu" ;;
    rpm) lib64=usr/lib64 lib32=usr/lib ;;
    arch) lib64=usr/lib lib32=usr/lib32 ;;
    apk) lib64=usr/lib lib32="" ;;
    *) echo "package.sh: no format ${FORMAT}" >&2; exit 2 ;;
esac
preload=/opt/selkies/lib/selkies/preload
link_interposers() {
    dir="$1" suffix="$2"
    [ -n "${dir}" ] && [ -f "${ROOT}${preload}/selkies_input_interposer${suffix}.so" ] || return 0
    mkdir -p "${ROOT}/${dir}"
    for name in selkies_input_interposer selkies_v4l2_interposer; do
        ln -s "${preload}/${name}${suffix}.so" "${ROOT}/${dir}/${name}.so"
    done
    ln -s selkies_input_interposer.so "${ROOT}/${dir}/selkies_joystick_interposer.so"
}
[ "${ARCH}" = x86_64 ] || lib32=""
link_interposers "${lib64}" ""
link_interposers "${lib32}" _32

case "${FORMAT}" in
    deb)
        DEB_ARCH="$(dpkg --print-architecture)"
        mkdir -p "${ROOT}/DEBIAN"
        cat > "${ROOT}/DEBIAN/control" <<EOF
Package: selkies
Version: $(tilde_version "${VERSION}")-1
Architecture: ${DEB_ARCH}
Maintainer: Selkies Project <pypi@linuxserver.io>
Installed-Size: $(du -sk "${ROOT}" | cut -f1)
Depends: libc6 (>= ${GLIBC}), libstdc++6, libgcc-s1 | libgcc1
Recommends: libgbm1, libegl1
Section: net
Priority: optional
Homepage: ${URL}
Description: ${SUMMARY}
EOF
        dpkg-deb --root-owner-group -Zxz --build "${ROOT}" "${OUT}/selkies-${VERSION}-${DEB_ARCH}.deb"
        ;;
    rpm)
        command -v rpmbuild > /dev/null || retry dnf install -y rpm-build
        mkdir -p "${WORK}/rpmbuild/SPECS"
        cat > "${WORK}/rpmbuild/SPECS/selkies.spec" <<EOF
Name: selkies
Version: $(tilde_version "${VERSION}")
Release: 1
Summary: ${SUMMARY}
License: MPL-2.0
URL: ${URL}
Requires: glibc >= ${GLIBC}, libstdc++, libgcc
Recommends: mesa-libgbm, mesa-libEGL
AutoReqProv: no
${RPM_PAYLOAD}
%global __os_install_post %{nil}
%global debug_package %{nil}
%global _build_id_links none

%description
${SUMMARY}.

%install
cp -a ${ROOT}/. %{buildroot}/

%files
/opt/selkies
/usr/bin/selkies
/usr/bin/selkies-desktop
/${lib64}/selkies_*.so
$([ -n "${lib32}" ] && [ -e "${ROOT}/${lib32}" ] && echo "/${lib32}/selkies_*.so")
EOF
        rpmbuild --define "_topdir ${WORK}/rpmbuild" -bb "${WORK}/rpmbuild/SPECS/selkies.spec"
        cp "${WORK}"/rpmbuild/RPMS/*/selkies-*.rpm "${OUT}/selkies-${VERSION}-${ARCH}.rpm"
        ;;
    apk)
        command -v abuild > /dev/null || retry apk add --no-cache abuild
        mkdir -p "${WORK}/apk"
        cat > "${WORK}/apk/APKBUILD" <<EOF
pkgname=selkies
pkgver=$(alpine_version "${VERSION}")
pkgrel=0
pkgdesc="${SUMMARY}"
url="${URL}"
arch="${ARCH}"
license="MPL-2.0"
depends=""
source=""
options="!check !strip !tracedeps !fhs"
package() {
    mkdir -p "\$pkgdir"
    cp -a "${ROOT}/." "\$pkgdir"/
}
EOF
        abuild-keygen -a -n
        cp -f "${HOME}"/.abuild/*.rsa.pub /etc/apk/keys/
        (cd "${WORK}/apk" && REPODEST="${WORK}/apkrepo" abuild -F rootpkg)
        apks="$(find "${WORK}/apkrepo" -name 'selkies-*.apk')"
        count="$(printf '%s\n' "${apks}" | grep -c .)" || true
        [ "${count}" -eq 1 ] || { echo "abuild produced ${count} .apk files, expected one" >&2; exit 1; }
        cp "${apks}" "${OUT}/selkies-${VERSION}-${ARCH}.apk"
        ;;
    arch)
        mkdir -p "${WORK}/arch"
        cat > "${WORK}/arch/PKGBUILD" <<EOF
pkgname=selkies
pkgver=$(printf '%s' "${VERSION}" | tr '-' '.')
pkgrel=1
pkgdesc="${SUMMARY}"
arch=("${ARCH}")
url="${URL}"
license=("MPL-2.0")
depends=("glibc" "gcc-libs")
optdepends=("mesa: GPU capture and encode" "libva: VA-API encode")
options=(!strip !debug)
package() {
    cp -a "${ROOT}/." "\$pkgdir"/
}
EOF
        id builder > /dev/null 2>&1 || useradd -m builder
        chown -R builder "${WORK}/arch"
        chmod -R a+rX "${WORK}"
        su builder -c "cd '${WORK}/arch' && PKGDEST='${WORK}/arch' makepkg -f --nodeps"
        cp "${WORK}"/arch/selkies-*.pkg.tar.zst "${OUT}/selkies-${VERSION}-${ARCH}.pkg.tar.zst"
        ;;
esac
ls -la "${OUT}"
