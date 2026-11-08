#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Build the selkies wheel for the platform of the manylinux2014 or
# musllinux_1_2 image this runs in, the images pixelflux's and pcmflux's own
# wheels are built in, and repair it so a plain `pip install` runs it there.
#
# The server is built by infra/packaging/server.sh --wheel, with every codec
# library linked in from the source pixelflux's wheels build (codecs.sh) and
# libxkbcommon linked in from a pinned release, since Enterprise Linux 7, the
# oldest host this platform tag admits, carries one older than the API the
# server reads keysyms through. What it still links is what pixelflux's and
# pcmflux's wheels link: auditwheel bundles what those bundle (libpulse and
# what it needs) and leaves the host what they leave it (libgbm, the graphics
# stack, pixman, the X11 client libraries, and libxcb), with the same
# exclusions.
#
# auditwheel moves an executable with bundled libraries into the package's
# platlib and starts it through a Python shim, because nothing about where pip
# puts scripts is relative to platlib. Here the libraries move instead, to
# the wheel's data at lib/selkies, which pip installs under the same prefix as
# the script: the executable keeps its place on PATH and runs without an
# interpreter, finds them at $ORIGIN/../lib/selkies, and finds its web client
# at share/selkies/web beside them.
#
# Usage: build.sh <out-dir>   (run from a checkout with dist/web built;
#                              SELKIES_VERSION stamps the version,
#                              SELKIES_SDIST=1 also writes the sdist)
set -eux
OUT="$(mkdir -p "${1:?usage: build.sh <out-dir>}" && readlink -f "$1")"
REPO="$(dirname "$(readlink -f "$0")")/../.."
BUILD="${SELKIES_BUILD_DIR:-/build}"
PYTHON=/opt/python/cp312-cp312/bin/python

"${REPO}/infra/packaging/portable-deps.sh" "maturin>=1.9,<2" wheel
PATH="$(dirname "${PYTHON}"):${PATH}"
export PATH

# libxkbcommon's parser takes bison 3.6 or newer, which CentOS 7 and
# AlmaLinux 8 predate (3.0.4): the release pinned here is built where it is.
bison="$(bison --version | head -n 1 | grep -oE '[0-9]+\.[0-9]+' | head -n 1)"
if [ "$(printf '%s\n3.6\n' "${bison}" | sort -V | head -n 1)" != 3.6 ]; then
    curl -fsSL --retry 5 -o /tmp/bison.tar.xz https://ftp.gnu.org/gnu/bison/bison-3.8.2.tar.xz
    echo "9bba0214ccf7f1079c5d59210045227bcf619519840ebfa80cd3849cff5a5bf2  /tmp/bison.tar.xz" | sha256sum -c -
    mkdir -p /tmp/bison
    tar -xJf /tmp/bison.tar.xz -C /tmp/bison --strip-components=1
    (cd /tmp/bison && ./configure --prefix=/usr/local > /dev/null && make -j"$(nproc)" > /dev/null \
        && make install > /dev/null)
    rm -rf /tmp/bison /tmp/bison.tar.xz
fi

XKB="${BUILD}/xkbcommon"
rm -rf "${XKB}"
git init -q "${XKB}/src"
git -C "${XKB}/src" fetch -q --depth 1 https://github.com/xkbcommon/libxkbcommon.git \
    refs/tags/xkbcommon-1.13.2:refs/tags/xkbcommon-1.13.2
[ "$(git -C "${XKB}/src" rev-parse 'FETCH_HEAD^{commit}')" = d1442aaa6b635551182e83c3b55037f4c118c962 ]
git -C "${XKB}/src" checkout -q FETCH_HEAD
meson setup "${XKB}/build" "${XKB}/src" --prefix="${XKB}" --libdir=lib --buildtype=release \
    --default-library=static -Db_staticpic=true -Denable-x11=false -Denable-wayland=false \
    -Denable-docs=false -Denable-tools=false -Denable-xkbregistry=false \
    -Dxkb-config-root=/usr/share/X11/xkb -Dx-locale-root=/usr/share/X11/locale
ninja -C "${XKB}/build" install
# The crates name the library to the linker rather than through pkg-config, so
# its directory goes ahead of the system's for the compiler driver as well
export LIBRARY_PATH="${XKB}/lib" PKG_CONFIG_PATH="${XKB}/lib/pkgconfig"

RAW="${BUILD}/wheel-raw"
rm -rf "${RAW}"
"${REPO}/infra/packaging/server.sh" --wheel "${RAW}"
if [ -n "${SELKIES_SDIST:-}" ]; then
    cp "${RAW}"/selkies-*.tar.gz "${OUT}/"
fi

# pixelflux's exclusions and pcmflux's: what the host provides for them
REPAIRED="${BUILD}/wheel-repaired"
rm -rf "${REPAIRED}"
auditwheel repair -w "${REPAIRED}" "${RAW}"/selkies-*.whl \
    --exclude libgbm.so.1 --exclude libEGL.so.1 --exclude libgobject-2.0.so.0 \
    --exclude libglib-2.0.so.0 --exclude libgmodule-2.0.so.0 --exclude libffi.so.6 \
    --exclude libffi.so.7 --exclude libffi.so.8 --exclude libxkbcommon.so.0 \
    --exclude libpixman-1.so.0 --exclude liblzma.so.5 --exclude libz.so.1 \
    --exclude libstdc++.so.6 --exclude libgcc_s.so.1 --exclude libX11.so.6 \
    --exclude libXext.so.6 --exclude libXfixes.so.3 --exclude libxcb.so.1 \
    --exclude libxcb-dri3.so.0 --exclude libxcb-shm.so.0 --exclude libxcb-render.so.0

# The executable back on PATH and its libraries under the same prefix
UNPACKED="${BUILD}/wheel-unpacked"
rm -rf "${UNPACKED}"
"${PYTHON}" -m wheel unpack -d "${UNPACKED}" "${REPAIRED}"/selkies-*.whl
TREE="$(echo "${UNPACKED}"/selkies-*)"
DATA="$(echo "${TREE}"/selkies-*.data)"
if [ -d "${TREE}/selkies.libs" ]; then
    mv "${TREE}/selkies.scripts/selkies" "${DATA}/scripts/selkies"
    rmdir "${TREE}/selkies.scripts"
    mkdir -p "${DATA}/data/lib"
    mv "${TREE}/selkies.libs" "${DATA}/data/lib/selkies"
    # shellcheck disable=SC2016  # the loader's $ORIGIN, literal
    patchelf --force-rpath --set-rpath '$ORIGIN/../lib/selkies' "${DATA}/scripts/selkies"
fi
"${PYTHON}" -m wheel pack -d "${OUT}" "${TREE}"
ls -la "${OUT}"
