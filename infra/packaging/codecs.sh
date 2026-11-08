#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Build the software codec libraries pixelflux links, at the releases its
# wheel recipe pins, as static archives the server links in, and write the
# environment that points the build at them.
#
# pixelflux binds libx264, x265, libvpx, SVT-AV1, dav1d, and libde265 from the
# copies it links. libx264, x265, libvpx, and SVT-AV1 carry the patches
# pixelflux keeps (its patches/x264, x265, libvpx, and svtav1): pixelflux reads
# the API the x264, x265, and SVT-AV1 ones add, and the libvpx ones copy only
# the blocks a frame's active map leaves active. Every one is built from the
# commit read out of the pixelflux checkout the server is built against, so the
# two never drift, and none is taken from a distribution: an archive from here
# is the same whichever distribution links it. It runs where a binary's C
# library floor is, the images the wheels are built in, once for each
# architecture and C library (scripts/ci/build-codecs.sh), and each native
# package links what that build left. A static archive in a prefix of its own
# is what pkg-config-rs and system-deps link statically when told to, which the
# written environment does per library.
#
# x264, x265, and libvpx compile with -ffast-math, under which a C library
# before glibc 2.31 redirects log, exp, pow, and the rest to __*_finite
# variants that 2.31 no longer lets a new link resolve: an archive from the
# floor would link nowhere newer. Undefining the __FINITE_MATH_ONLY__ the
# compiler predefines for it keeps the header from redirecting, whatever order
# a build puts its own flags in, and leaves the fast-math code generation alone.
#
# Usage: codecs.sh <prefix> <pixelflux crate directory>
#   Writes <prefix>/env.sh, to be sourced before `cargo build`.
set -eu
PREFIX="${1:?usage: codecs.sh <prefix> <pixelflux crate directory>}"
CRATE="${2:?usage: codecs.sh <prefix> <pixelflux crate directory>}"
RECIPE="${CRATE}/pyproject.toml"
JOBS="$(nproc 2>/dev/null || echo 4)"
CFLAGS="${CFLAGS:+${CFLAGS} }-U__FINITE_MATH_ONLY__"
CXXFLAGS="${CXXFLAGS:+${CXXFLAGS} }-U__FINITE_MATH_ONLY__"
export CFLAGS CXXFLAGS
mkdir -p "${PREFIX}/src"
ENV_FILE="${PREFIX}/env.sh"
: > "${ENV_FILE}"

# The commit, tag, and mirrors pixelflux's wheel recipe clones `name` from.
recipe_source() {
    awk -v name="$1" '
        $1 == "clone_source" && $4 == name { pick = $2 " " $3; next }
        pick != "" { sub(/ *&&.*/, ""); sub(/^ +/, ""); print pick " " $0; exit }
    ' "${RECIPE}"
}

# Fetch the pinned source of `name` into src/`name`, refusing a tag that moved.
fetch() {
    # shellcheck disable=SC2046  # the commit, tag, and mirrors are separate words
    set -- $(recipe_source "$1") "$1"
    [ "$#" -ge 4 ] || { echo "codecs: pixelflux's recipe names no source for ${4:-$1}" >&2; exit 1; }
    commit="$1" tag="$2"
    shift 2
    urls=""
    while [ "$#" -gt 1 ]; do urls="${urls} $1"; shift; done
    dir="${PREFIX}/src/$1"
    want="${commit}"
    [ "${tag}" = - ] || want="refs/tags/${tag}:refs/tags/${tag}"
    for url in ${urls}; do
        for attempt in 1 2 3 4; do
            rm -rf "${dir}"
            if git init -q "${dir}" && git -C "${dir}" fetch -q --depth 1 "${url}" "${want}"; then
                got="$(git -C "${dir}" rev-parse 'FETCH_HEAD^{commit}')"
                [ "${got}" = "${commit}" ] || { echo "codecs: ${url} has ${tag} at ${got}, not ${commit}" >&2; exit 1; }
                git -C "${dir}" checkout -q "${commit}"
                return 0
            fi
            sleep $((attempt * 15))
        done
    done
    echo "codecs: no mirror served $1" >&2
    exit 1
}

# Interprocedural optimization off, which SVT-AV1 turns on wherever the
# compiler offers it: the archive would then hold GCC's intermediate code,
# which the linker Rust drives cannot read.
cmake_static() {
    src="$1" build="$2"
    shift 2
    cmake -S "${src}" -B "${build}" -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX="${PREFIX}" \
        -DCMAKE_INSTALL_LIBDIR=lib -DBUILD_SHARED_LIBS=OFF -DCMAKE_POSITION_INDEPENDENT_CODE=ON \
        -DCMAKE_INTERPROCEDURAL_OPTIMIZATION=OFF "$@"
}

# pixelflux reads the API its patches add, so x264 is always built, with them.
build_x264() {
    fetch x264
    git -C "${PREFIX}/src/x264" apply "${CRATE}"/patches/x264/*.patch
    (cd "${PREFIX}/src/x264" && ./configure --prefix="${PREFIX}" --enable-static --enable-pic \
        --disable-cli --disable-opencl && make -j"${JOBS}" && make install)
}

# The 10-bit encoder is linked into the 8-bit one, as pixelflux's wheels carry
# it, so x265_api_get(10) answers; the two archives are merged into one.
build_x265() {
    fetch x265
    src="${PREFIX}/src/x265"
    git -C "${src}" apply "${CRATE}"/patches/x265/*.patch
    cmake -S "${src}/source" -B "${src}/build10" -DCMAKE_BUILD_TYPE=Release -DHIGH_BIT_DEPTH=ON \
        -DEXPORT_C_API=OFF -DENABLE_SHARED=OFF -DENABLE_CLI=OFF -DCMAKE_POSITION_INDEPENDENT_CODE=ON
    cmake --build "${src}/build10" -j"${JOBS}"
    mkdir -p "${src}/build"
    cp "${src}/build10/libx265.a" "${src}/build/libx265_main10.a"
    cmake -S "${src}/source" -B "${src}/build" -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_INSTALL_PREFIX="${PREFIX}" -DCMAKE_INSTALL_LIBDIR=lib -DENABLE_SHARED=OFF \
        -DENABLE_CLI=OFF -DCMAKE_POSITION_INDEPENDENT_CODE=ON -DEXTRA_LIB=x265_main10.a \
        -DEXTRA_LINK_FLAGS="-L${src}/build" -DLINKED_10BIT=ON
    cmake --build "${src}/build" -j"${JOBS}"
    cmake --install "${src}/build"
    (cd "${src}/build" && printf 'CREATE libx265_all.a\nADDLIB %s/lib/libx265.a\nADDLIB libx265_main10.a\nSAVE\nEND\n' \
        "${PREFIX}" | ar -M && mv libx265_all.a "${PREFIX}/lib/libx265.a")
}

build_libvpx() {
    fetch libvpx
    git -C "${PREFIX}/src/libvpx" apply "${CRATE}"/patches/libvpx/*.patch
    as=""
    [ "$(uname -m)" != x86_64 ] || as="--as=nasm"
    # shellcheck disable=SC2086  # as is one flag or none
    (cd "${PREFIX}/src/libvpx" && ./configure --prefix="${PREFIX}" --enable-static --disable-shared \
        --enable-pic --disable-examples --disable-tools --disable-docs --disable-unit-tests \
        --enable-vp9-highbitdepth ${as} && make -j"${JOBS}" && make install)
}

build_svtav1() {
    fetch svtav1
    git -C "${PREFIX}/src/svtav1" apply "${CRATE}"/patches/svtav1/*.patch
    cmake_static "${PREFIX}/src/svtav1" "${PREFIX}/src/svtav1/build" -DBUILD_APPS=OFF -DBUILD_DEC=OFF
    cmake --build "${PREFIX}/src/svtav1/build" -j"${JOBS}"
    cmake --install "${PREFIX}/src/svtav1/build"
}

build_dav1d() {
    fetch dav1d
    (cd "${PREFIX}/src/dav1d" && meson setup build --prefix="${PREFIX}" --libdir=lib \
        --buildtype=release --default-library=static -Db_staticpic=true \
        -Denable_tools=false -Denable_tests=false && ninja -C build && ninja -C build install)
}

build_libde265() {
    fetch libde265
    cmake_static "${PREFIX}/src/libde265" "${PREFIX}/src/libde265/build" -DENABLE_SDL=OFF \
        -DENABLE_DECODER=OFF -DENABLE_ENCODER=OFF
    cmake --build "${PREFIX}/src/libde265/build" -j"${JOBS}"
    cmake --install "${PREFIX}/src/libde265/build"
}

if [ ! -f "${RECIPE}" ]; then
    echo "codecs: ${CRATE} holds no pixelflux wheel recipe" >&2
    exit 1
fi

# recipe name and the variable that links it statically, which pkg-config-rs
# reads as <NAME>_STATIC
for entry in x264:X264_STATIC=1 x265:X265_STATIC=1 libvpx:VPX_STATIC=1 \
        svtav1:SVTAV1ENC_STATIC=1 dav1d:DAV1D_STATIC=1 libde265:LIBDE265_STATIC=1; do
    echo "codecs: building ${entry%%:*}"
    "build_${entry%%:*}"
    echo "export ${entry#*:}" >> "${ENV_FILE}"
done
echo "export PKG_CONFIG_PATH=\"${PREFIX}/lib/pkgconfig\${PKG_CONFIG_PATH:+:\${PKG_CONFIG_PATH}}\"" >> "${ENV_FILE}"
