#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Build the software codec libraries this distribution does not carry at the
# release pixelflux needs, as static archives the server links in, and write
# the environment that points the build at them.
#
# pixelflux binds libx264, x265, libvpx, SVT-AV1, dav1d, and libde265 from the
# copies it links. libx264 is always built here, with the patches pixelflux
# carries (its patches/x264), since pixelflux reads the API they add. Each one the distribution carries at the floor pixelflux's
# codec-sys names (pkg-config answers for it) is linked from the distribution
# and becomes a package dependency; each one it lacks (x264, x265, and
# libde265 on Fedora, all of them on EL and on the AppImage's glibc floor) is
# built here from the source pixelflux's own wheels build, the commit read out
# of the pixelflux checkout the server is built against so the two never drift,
# and linked statically, since no package on the target system would provide
# it. A static archive in a prefix of its own is what pkg-config-rs and
# system-deps link statically when told to, which the written environment does
# per library.
#
# Usage: codecs.sh <prefix> <pixelflux crate directory>
#   Writes <prefix>/env.sh, to be sourced before `cargo build`.
set -eu
PREFIX="${1:?usage: codecs.sh <prefix> <pixelflux crate directory>}"
CRATE="${2:?usage: codecs.sh <prefix> <pixelflux crate directory>}"
RECIPE="${CRATE}/pyproject.toml"
FLOORS="${CRATE}/codec-sys/build.rs"
JOBS="$(nproc 2>/dev/null || echo 4)"
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

# The floor codec-sys holds the pkg-config name `pkg` to, empty where it names none.
floor() {
    awk -v pkg="$1" '
        $1 == "pkg:" { gsub(/[",]/, "", $2); current = $2 }
        $1 == "min:" && current == pkg { gsub(/[",]/, "", $2); print $2; exit }
    ' "${FLOORS}"
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
    as=""
    [ "$(uname -m)" != x86_64 ] || as="--as=nasm"
    # shellcheck disable=SC2086  # as is one flag or none
    (cd "${PREFIX}/src/libvpx" && ./configure --prefix="${PREFIX}" --enable-static --disable-shared \
        --enable-pic --disable-examples --disable-tools --disable-docs --disable-unit-tests \
        --enable-vp9-highbitdepth ${as} && make -j"${JOBS}" && make install)
}

build_svtav1() {
    fetch svtav1
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

if [ ! -f "${RECIPE}" ] || [ ! -f "${FLOORS}" ]; then
    echo "codecs: ${CRATE} holds no pixelflux wheel recipe and codec-sys" >&2
    exit 1
fi

# recipe name, pkg-config name, and the variable that links it statically:
# pkg-config-rs reads <NAME>_STATIC, system-deps (x264-sys) its own spelling
built=""
for entry in x264:x264:SYSTEM_DEPS_X264_LINK=static x265:x265:X265_STATIC=1 \
        libvpx:vpx:VPX_STATIC=1 svtav1:SvtAv1Enc:SVTAV1ENC_STATIC=1 \
        dav1d:dav1d:DAV1D_STATIC=1 libde265:libde265:LIBDE265_STATIC=1; do
    name="${entry%%:*}"
    rest="${entry#*:}"
    pkg="${rest%%:*}"
    static="${rest#*:}"
    min="$(floor "${pkg}")"
    if [ "${name}" != x264 ] && pkg-config --exists "${pkg}" && pkg-config --atleast-version="${min:-0}" "${pkg}"; then
        echo "codecs: ${pkg} $(pkg-config --modversion "${pkg}") from the distribution"
        continue
    fi
    echo "codecs: ${pkg} ${min:+${min} or newer }is not in the distribution; building it"
    "build_${name}"
    echo "export ${static}" >> "${ENV_FILE}"
    built="${built} ${pkg}"
done
if [ -n "${built}" ]; then
    echo "export PKG_CONFIG_PATH=\"${PREFIX}/lib/pkgconfig\${PKG_CONFIG_PATH:+:\${PKG_CONFIG_PATH}}\"" >> "${ENV_FILE}"
    echo "codecs: linked statically:${built}"
fi
