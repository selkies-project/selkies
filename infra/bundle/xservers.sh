#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Build the bundle's X servers into <tree>: XLibre's Xvfb with the Selkies
# patches (addons/base/patches), the upstream Xwayland (XLibre carries none
# since it dropped Xwayland), the xkbcomp both of them run, and the
# xkeyboard-config data they and the server's xkbcommon read.
#
# The servers run beside the host's GPU driver in one process (glamor loads the
# host's EGL, and with it the libraries that driver links), so they carry no
# shared library of their own that a driver could also want in a newer version:
# every dependency is built here from its pinned release as a static archive
# and linked in, the only shared libraries they need are glibc's, and libgbm,
# which only the host can supply, is reached through a loader that opens it at
# run time and leaves the server in software where the host has none
# (gbm-loader.c). They find their xkb data and xkbcomp relative to their own
# directory (patches/*-relative-xkb.patch), their font path is the built-in
# fonts, and their compiled keymaps go to the runtime directory, so the tree
# runs wherever it is unpacked.
#
# Runs inside the manylinux_2_28 image infra/bundle/build.sh starts, whose glibc
# is the bundle's floor. Each source is fetched once into SELKIES_SOURCE_CACHE
# (default <build>/sources) and checked against the digest pinned here.
#
# Usage: xservers.sh <tree>
#   writes <tree>/libexec/selkies/{Xvfb,Xwayland,xkbcomp} and <tree>/share/selkies/xkb
set -eux
TREE="$(mkdir -p "${1:?usage: xservers.sh <tree>}" && readlink -f "$1")"
REPO="$(dirname "$(readlink -f "$0")")/../.."
BUILD="${SELKIES_BUILD_DIR:-/build}/xservers"
CACHE="${SELKIES_SOURCE_CACHE:-${BUILD}/sources}"
DEPS="${BUILD}/deps"
JOBS="$(nproc)"
mkdir -p "${BUILD}/src" "${CACHE}" "${DEPS}"

# The build tools and headers come from infra/bundle/deps.sh
export PATH="/opt/python/cp312-cp312/bin:${PATH}"
export PKG_CONFIG_PATH="${DEPS}/lib/pkgconfig:${DEPS}/share/pkgconfig"
export CFLAGS="-O2 -fPIC" CXXFLAGS="-O2 -fPIC"
export ACLOCAL_PATH="${DEPS}/share/aclocal"

# fetch <url> <sha256>: the source, unpacked under src/, its directory printed
fetch() {
    name="$(basename "$1")"
    if [ ! -f "${CACHE}/${name}" ]; then
        curl -fsSL --retry 5 --connect-timeout 30 -o "${CACHE}/${name}.part" "$1"
        mv "${CACHE}/${name}.part" "${CACHE}/${name}"
    fi
    echo "$2  ${CACHE}/${name}" | sha256sum -c - >&2
    dir="${BUILD}/src/${3:-$(echo "${name}" | sed -E 's/\.tar\.(gz|xz)$//')}"
    rm -rf "${dir}"
    mkdir -p "${dir}"
    tar -xf "${CACHE}/${name}" -C "${dir}" --strip-components=1
    echo "${dir}"
}

# autotools <dir> [configure args]: a static archive installed under DEPS
autotools() {
    dir="$1"
    shift
    (cd "${dir}" && ./configure --prefix="${DEPS}" --libdir="${DEPS}/lib" \
        --enable-static --disable-shared --with-pic "$@" > /dev/null \
        && make -j"${JOBS}" > /dev/null && make install > /dev/null)
}

# mesonbuild <dir> [meson args]: the same through meson
mesonbuild() {
    dir="$1"
    shift
    meson setup "${dir}/build" "${dir}" --prefix="${DEPS}" --libdir=lib \
        --buildtype=release --default-library=static -Db_staticpic=true \
        -Dprefer_static=true --wrap-mode=nodownload "$@"
    ninja -C "${dir}/build" install > /dev/null
}

# The servers as an earlier run built them, where nothing they are built from
# has changed since
KEY="$(cat "$0" "${REPO}"/infra/bundle/*-loader.c "${REPO}"/infra/bundle/patches/*.patch \
    "${REPO}"/addons/base/patches/xvfb-*.patch | sha256sum | cut -c1-16)"
BUILT="${BUILD}/built-${KEY}"
if [ -d "${BUILT}" ]; then
    mkdir -p "${TREE}/libexec/selkies" "${TREE}/share/selkies"
    cp -a "${BUILT}/libexec/selkies/." "${TREE}/libexec/selkies/"
    rm -rf "${TREE}/share/selkies/xkb"
    cp -a "${BUILT}/share/selkies/xkb" "${TREE}/share/selkies/xkb"
    exit 0
fi

# The static dependencies, once per build directory: a rerun after a failure in
# the servers below starts from them.
if [ ! -f "${DEPS}/.complete" ]; then
d="$(fetch https://github.com/madler/zlib/releases/download/v1.3.1/zlib-1.3.1.tar.gz \
    9a93b2b7dfdac77ceba5a558a580e74667dd6fede4585b91eefb60f03b72df23)"
(cd "${d}" && CFLAGS="${CFLAGS}" ./configure --prefix="${DEPS}" --libdir="${DEPS}/lib" --static \
    > /dev/null && make -j"${JOBS}" > /dev/null && make install > /dev/null)
d="$(fetch https://github.com/libffi/libffi/releases/download/v3.4.6/libffi-3.4.6.tar.gz \
    b0dea9df23c863a7a50e825440f3ebffabd65df1497108e5d437747843895a4e)"
autotools "${d}" --disable-docs --disable-multi-os-directory
d="$(fetch https://ftp.gnu.org/gnu/nettle/nettle-3.10.1.tar.gz \
    b0fcdd7fc0cdea6e80dcf1dd85ba794af0d5b4a57e26397eee3bc193272d9132)"
autotools "${d}" --disable-public-key --disable-openssl --disable-documentation
d="$(fetch https://xorg.freedesktop.org/archive/individual/proto/xorgproto-2024.1.tar.xz \
    372225fd40815b8423547f5d890c5debc72e88b91088fbfb13158c20495ccb59)"
mesonbuild "${d}"
d="$(fetch https://xorg.freedesktop.org/archive/individual/proto/xcb-proto-1.17.0.tar.xz \
    2c1bacd2110f4799f74de6ebb714b94cf6f80fb112316b1219480fd22562148c)"
(cd "${d}" && ./configure --prefix="${DEPS}" > /dev/null && make install > /dev/null)
d="$(fetch https://xorg.freedesktop.org/archive/individual/lib/xtrans-1.6.0.tar.xz \
    faafea166bf2451a173d9d593352940ec6404145c5d1da5c213423ce4d359e92)"
autotools "${d}" --disable-docs
d="$(fetch https://xorg.freedesktop.org/archive/individual/lib/libXau-1.0.12.tar.xz \
    74d0e4dfa3d39ad8939e99bda37f5967aba528211076828464d2777d477fc0fb)"
autotools "${d}"
d="$(fetch https://xorg.freedesktop.org/archive/individual/lib/libXdmcp-1.1.5.tar.xz \
    d8a5222828c3adab70adf69a5583f1d32eb5ece04304f7f8392b6a353aa2228c)"
autotools "${d}" --disable-docs
d="$(fetch https://xorg.freedesktop.org/archive/individual/lib/libxcb-1.17.0.tar.xz \
    599ebf9996710fea71622e6e184f3a8ad5b43d0e5fa8c4e407123c88a59a6d55)"
autotools "${d}" --disable-devel-docs --without-doxygen
d="$(fetch https://xorg.freedesktop.org/archive/individual/lib/libX11-1.8.12.tar.xz \
    fa026f9bb0124f4d6c808f9aef4057aad65e7b35d8ff43951cef0abe06bb9a9a)"
autotools "${d}" --disable-specs --disable-xf86bigfont --without-xmlto --without-fop
d="$(fetch https://xorg.freedesktop.org/archive/individual/lib/libxshmfence-1.3.3.tar.xz \
    d4a4df096aba96fea02c029ee3a44e11a47eb7f7213c1a729be83e85ec3fde10)"
autotools "${d}"
d="$(fetch https://xorg.freedesktop.org/archive/individual/lib/libfontenc-1.1.8.tar.xz \
    7b02c3d405236e0d86806b1de9d6868fe60c313628b38350b032914aa4fd14c6)"
autotools "${d}"
d="$(fetch https://xorg.freedesktop.org/archive/individual/lib/libXfont2-2.0.7.tar.xz \
    8b7b82fdeba48769b69433e8e3fbb984a5f6bf368b0d5f47abeec49de3e58efb)"
autotools "${d}" --disable-freetype --disable-devel-docs --disable-selective-werror
d="$(fetch https://xorg.freedesktop.org/archive/individual/lib/libxkbfile-1.1.3.tar.xz \
    a9b63eea997abb9ee6a8b4fbb515831c841f471af845a09de443b28003874bec)"
mesonbuild "${d}"
d="$(fetch https://xorg.freedesktop.org/archive/individual/lib/libxcvt-0.1.3.tar.xz \
    a929998a8767de7dfa36d6da4751cdbeef34ed630714f2f4a767b351f2442e01)"
mesonbuild "${d}"
# libxcvt's build makes a shared library alone, so its one file is archived here
# shellcheck disable=SC2086  # CFLAGS is a list of flags
cc ${CFLAGS} -I"${d}/include" -c "${d}/lib/libxcvt.c" -o "${BUILD}/libxcvt.o"
ar rcs "${DEPS}/lib/libxcvt.a" "${BUILD}/libxcvt.o"
rm -f "${DEPS}"/lib/libxcvt.so*
d="$(fetch https://www.cairographics.org/releases/pixman-0.46.4.tar.gz \
    d09c44ebc3bd5bee7021c79f922fe8fb2fb57f7320f55e97ff9914d2346a591c)"
mesonbuild "${d}" -Dtests=disabled -Ddemos=disabled -Dgtk=disabled -Dlibpng=disabled
d="$(fetch https://dri.freedesktop.org/libdrm/libdrm-2.4.125.tar.xz \
    d4bae92797a50f81a93524762e0410a49cd84cfa0f997795bc0172ac8fb1d96a)"
mesonbuild "${d}" -Dintel=disabled -Dradeon=disabled -Damdgpu=disabled -Dnouveau=disabled \
    -Dvmwgfx=disabled -Dfreedreno=disabled -Dvc4=disabled -Detnaviv=disabled -Dtests=false \
    -Dman-pages=disabled -Dvalgrind=disabled -Dcairo-tests=disabled -Dudev=false
d="$(fetch https://github.com/anholt/libepoxy/archive/refs/tags/1.5.10.tar.gz \
    a7ced37f4102b745ac86d6a70a9da399cc139ff168ba6b8002b4d8d43c900c15 libepoxy-1.5.10)"
mesonbuild "${d}" -Dglx=no -Dx11=false -Degl=yes -Dtests=false -Ddocs=false
d="$(fetch https://gitlab.freedesktop.org/wayland/wayland/-/releases/1.23.1/downloads/wayland-1.23.1.tar.xz \
    864fb2a8399e2d0ec39d56e9d9b753c093775beadc6022ce81f441929a81e5ed)"
mesonbuild "${d}" -Ddocumentation=false -Dtests=false -Ddtd_validation=false
d="$(fetch https://gitlab.freedesktop.org/wayland/wayland-protocols/-/releases/1.44/downloads/wayland-protocols-1.44.tar.xz \
    3df1107ecf8bfd6ee878aeca5d3b7afd81248a48031e14caf6ae01f14eebb50e)"
mesonbuild "${d}" -Dtests=false
touch "${DEPS}/.complete"
fi

# libwayland-server as a shared library as well, which the server opens at run
# time (wayland-sys's dlopen) and the bundle carries for a host without one
if [ ! -f "${DEPS}/lib/libwayland-server.so.0" ]; then
    d="$(fetch https://gitlab.freedesktop.org/wayland/wayland/-/releases/1.23.1/downloads/wayland-1.23.1.tar.xz \
        864fb2a8399e2d0ec39d56e9d9b753c093775beadc6022ce81f441929a81e5ed)"
    meson setup "${d}/build" "${d}" --prefix="${DEPS}" --libdir=lib --buildtype=release \
        --default-library=both -Db_staticpic=true --wrap-mode=nodownload \
        -Ddocumentation=false -Dtests=false -Ddtd_validation=false
    ninja -C "${d}/build" install > /dev/null
fi

# The libgbm loader, found by pkg-config in place of the host's libgbm
cc -O2 -fPIC -c "${REPO}/infra/bundle/gbm-loader.c" -o "${BUILD}/gbm-loader.o"
ar rcs "${DEPS}/lib/libgbm.a" "${BUILD}/gbm-loader.o"
gbm_version="$(pkg-config --modversion --with-path=/usr/lib64/pkgconfig gbm 2> /dev/null || echo 23.1.4)"
cat > "${DEPS}/lib/pkgconfig/gbm.pc" <<EOF
prefix=${DEPS}
libdir=\${prefix}/lib
Name: gbm
Description: libgbm, loaded from the host at run time
Version: ${gbm_version}
Libs: -L\${libdir} -lgbm
Libs.private: -ldl -lpthread
Cflags:
EOF

# GL through libepoxy, which resolves every entry point at run time: the
# servers' indirect GLX dispatch calls GL directly, and linking the host's
# libGL for it would leave a host without one unable to start the server.
echo '#include <epoxy/gl.h>' > "${DEPS}/include/GL/gl.h"
cp "${DEPS}/include/GL/gl.h" "${DEPS}/include/GL/glext.h"
cc -O2 -fPIC -c "${REPO}/infra/bundle/gl-loader.c" -o "${BUILD}/gl-loader.o"
ar rcs "${DEPS}/lib/libselkies-gl.a" "${BUILD}/gl-loader.o"
cat > "${DEPS}/lib/pkgconfig/gl.pc" <<EOF
prefix=${DEPS}
libdir=\${prefix}/lib
includedir=\${prefix}/include
Name: gl
Description: GL through libepoxy, and glXGetProcAddress from the host at run time
Version: 1.2
Requires: epoxy
Libs: -L\${libdir} -lselkies-gl
Libs.private: -ldl -lpthread
Cflags: -I\${includedir}
EOF

# Paths the servers resolve against their own directory, libexec/selkies
XKB_ARGS="-Dxkb_dir=../../share/selkies/xkb -Dxkb_bin_dir=. -Dxkb_output_dir=/var/lib/xkb \
    -Ddefault_font_path=built-ins -Dsha1=libnettle -Dxdmcp=false -Dxdm-auth-1=false \
    -Dxselinux=false -Ddtrace=false -Dlibunwind=false -Ddocs=false -Ddevel-docs=false"

d="$(fetch https://xorg.freedesktop.org/archive/individual/xserver/xwayland-24.1.15.tar.xz \
    7311b4bcda22cfc19cb99680eb1c35cfc1ea885981310639f0429fe2948c122e)"
patch -d "${d}" -p1 < "${REPO}/infra/bundle/patches/xwayland-relative-xkb.patch"
# shellcheck disable=SC2086  # XKB_ARGS is a list of options
meson setup "${d}/build" "${d}" --buildtype=release -Dprefer_static=true --wrap-mode=nodownload \
    ${XKB_ARGS} -Dxvfb=false -Dglamor=true -Ddri3=true -Dglx=true -Dxwayland_ei=false \
    -Dlibdecor=false -Dsecure-rpc=false -Dsystemd_notify=false -Ddocs-pdf=false -Dxcsecurity=true
ninja -C "${d}/build" hw/xwayland/Xwayland
install -Dm755 "${d}/build/hw/xwayland/Xwayland" "${TREE}/libexec/selkies/Xwayland"

XLIBRE_TAG="xlibre-xserver-25.2.2"
d="$(fetch "https://github.com/X11Libre/xserver/archive/refs/tags/${XLIBRE_TAG}.tar.gz" \
    3c1f07f4b5f8a483473097be4b964ee97a2b92adde9de6c0d8d9d7217a382810 xlibre)"
for p in "${REPO}"/addons/base/patches/xvfb-*.patch "${REPO}"/infra/bundle/patches/xlibre-*.patch; do
    patch -d "${d}" -p1 < "${p}"
done
# shellcheck disable=SC2086  # XKB_ARGS is a list of options
meson setup "${d}/build" "${d}" --buildtype=release -Dprefer_static=true --wrap-mode=nodownload \
    ${XKB_ARGS} -Dxvfb=true -Dglamor=true -Dgbm=true -Ddri3=true -Dglx=true -Dxcsecurity=true \
    -Dxorg=false -Dxnest=false -Dxephyr=false -Dxwin=false -Dxquartz=false -Dxfbdev=false \
    -Dudev=false -Dudev_kms=false -Dsystemd_logind=false -Dseatd_libseat=false -Dpciaccess=false \
    -Dtests=false
ninja -C "${d}/build" hw/vfb/Xvfb
install -Dm755 "${d}/build/hw/vfb/Xvfb" "${TREE}/libexec/selkies/Xvfb"
usage="$("${TREE}/libexec/selkies/Xvfb" -help 2>&1 || true)"
grep -q -- '-glamor' <<EOF
${usage}
EOF

d="$(fetch https://xorg.freedesktop.org/archive/individual/app/xkbcomp-1.4.7.tar.xz \
    0a288114e5f44e31987042c79aecff1ffad53a8154b8ec971c24a69a80f81f77)"
(cd "${d}" && ./configure --prefix="${DEPS}" PKG_CONFIG="pkg-config --static" > /dev/null \
    && make -j"${JOBS}" > /dev/null)
install -Dm755 "${d}/xkbcomp" "${TREE}/libexec/selkies/xkbcomp"

d="$(fetch https://xorg.freedesktop.org/archive/individual/data/xkeyboard-config/xkeyboard-config-2.44.tar.xz \
    54d2c33eeebb031d48fa590c543e54c9bcbd0f00386ebc6489b2f47a0da4342a)"
meson setup "${d}/build" "${d}" --prefix="${BUILD}/xkb" -Dxorg-rules-symlinks=true \
    -Dcompat-rules=true -Dnls=false
ninja -C "${d}/build" install > /dev/null
rm -rf "${TREE}/share/selkies/xkb"
mkdir -p "${TREE}/share/selkies"
cp -a "${BUILD}/xkb/share/X11/xkb" "${TREE}/share/selkies/xkb"
test -f "${TREE}/share/selkies/xkb/rules/evdev"

# Nothing but glibc: anything else is a copy the host's driver could clash with in
# the server's process, or a library a bare host lacks
for bin in Xvfb Xwayland xkbcomp; do
    other="$(readelf -d "${TREE}/libexec/selkies/${bin}" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p' \
        | grep -vxE 'lib(c|m|dl|pthread|rt)\.so\.[0-9]+|ld-linux[-a-z0-9_]*\.so\.[0-9]+' || true)"
    [ -z "${other}" ] || { echo "${bin} links ${other}" >&2; exit 1; }
done

rm -rf "${BUILD}"/built-*
mkdir -p "${BUILT}/libexec/selkies" "${BUILT}/share/selkies"
for bin in Xvfb Xwayland xkbcomp; do
    cp -a "${TREE}/libexec/selkies/${bin}" "${BUILT}/libexec/selkies/"
done
cp -a "${TREE}/share/selkies/xkb" "${BUILT}/share/selkies/xkb"
