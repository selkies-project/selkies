#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Name the bundle's toolchain image: the build image infra/bundle/Dockerfile
# makes for one C library and architecture, holding everything a bundle carries
# or links that this tree does not compile itself (the build tools and headers,
# the Rust toolchain, the X servers and their static dependencies, CUPS, the
# codecs, libxkbcommon, PipeWire's headers, dbus-daemon, and NVIDIA's X11 EGL
# platform), so a bundle build compiles only the server, the shell, and the
# interposers.
#
# The image is named for its inputs rather than for a branch: its tag is the C
# library, the architecture, and a digest of every file the Dockerfile copies
# (`files`), the Dockerfile with its builder images pinned by digest among them,
# so an image of that tag can only have been built from those files. A build
# that finds it, locally or in the registry, takes it as it is (`ensure`); one
# that does not builds it, which Docker's layer cache keeps to the stages whose
# files changed. Only main publishes one (.github/workflows/ci.yaml, `toolchain`),
# so a pull request that changes what it is built from builds its own.
# Of pixelflux's wheel recipe only the codec sources it pins count
# (codecs.sh reads nothing else of it), so an edit elsewhere in it moves no tag.
#
# The Dockerfile's stage for a target, `<libc>-<arch>`, is the one place its
# builder image is chosen, and that image's C library is the floor of the
# target's bundle and of everything repackaged from it: `base` and `platform`
# read it from there for the wheel's tag (cargo xtask wheel), the packages'
# C library dependency (infra/packaging/package.sh), and the hosts CI checks
# the floor on.
#
# Usage: toolchain.sh files                    the inputs, one path per line
#        toolchain.sh key                      the digest of the inputs
#        toolchain.sh image <libc>             the image for <libc> on this
#                                              architecture
#        toolchain.sh ensure <libc>            that image, present locally,
#                                              pulled, or built, printed once
#                                              it is here
#        toolchain.sh base <libc> [arch]       the builder image of <libc> on
#                                              <arch> (this one by default)
#        toolchain.sh platform <libc> [arch]   the platform its bundle runs on:
#                                              manylinux_2_17, manylinux_2_28,
#                                              or musllinux_1_2
#   SELKIES_TOOLCHAIN_REPOSITORY overrides the repository the images are named in.
set -eu
cd "$(dirname "$(readlink -f "$0")")/../.."
REPOSITORY="${SELKIES_TOOLCHAIN_REPOSITORY:-ghcr.io/selkies-project/selkies/bundle-toolchain}"

files() {
    printf '%s\n' infra/bundle/Dockerfile infra/bundle/toolchain.sh infra/bundle/deps.sh \
        infra/packaging/portable-deps.sh crates/pcmflux/scripts/build-libpulse.sh \
        infra/packaging/rust.sh infra/packaging/xkbcommon.sh infra/bundle/pipewire.sh \
        infra/bundle/egl-x11.sh infra/bundle/dbus.sh infra/packaging/codecs.sh \
        crates/pixelflux/pyproject.toml \
        infra/bundle/xservers.sh infra/bundle/gbm-loader.c infra/bundle/gl-loader.c \
        infra/bundle/cups.sh
    find crates/pixelflux/patches/x264 crates/pixelflux/patches/x265 \
        crates/pixelflux/patches/libvpx crates/pixelflux/patches/svtav1 infra/bundle/patches \
        addons/base/patches -type f \( -path 'addons/base/patches/*' -name 'xvfb-*.patch' -o \
        ! -path 'addons/base/patches/*' \) | LC_ALL=C sort
}

key() {
    files | while read -r file; do
        echo "${file}"
        case "${file}" in
            crates/pixelflux/pyproject.toml)
                awk '$1 == "clone_source" && NF > 3 { print; getline; print }' "${file}" ;;
            *) cat "${file}" ;;
        esac
    done | sha256sum | cut -c1-16
}

image() {
    case "$1" in
        glibc | musl) echo "${REPOSITORY}:${1}-$(uname -m)-$(key)" ;;
        *) echo "toolchain: no C library $1" >&2; exit 2 ;;
    esac
}

# A pull that fails for any reason builds the image: the build is slower than
# the pull, never different from it.
ensure() {
    name="$(image "$1")"
    if ! docker image inspect "${name}" > /dev/null 2>&1 && ! docker pull -q "${name}" > /dev/null; then
        echo "toolchain: ${name} is not published; building it" >&2
        files | tar -c -T - | docker build -f infra/bundle/Dockerfile \
            --build-arg TARGET="$1-$(uname -m)" -t "${name}" - >&2
    fi
    echo "${name}"
}

base() {
    stage="$1-$2"
    from="$(sed -n "s/^FROM \([^ ]*\) AS ${stage}\$/\1/p" infra/bundle/Dockerfile)"
    [ -n "${from}" ] || { echo "toolchain: no builder image for ${stage}" >&2; exit 2; }
    echo "${from}"
}

# The builder image's name without its architecture, which manylinux names its
# glibc by: manylinux2014 is manylinux_2_17 (PEP 599)
platform() {
    name="$(base "$1" "$2")"
    name="${name##*/}"
    name="${name%%[:@]*}"
    name="${name%_"$2"}"
    if [ "${name}" = manylinux2014 ]; then name=manylinux_2_17; fi
    echo "${name}"
}

case "${1:-}" in
    files) files ;;
    key) key ;;
    image) image "${2:-}" ;;
    ensure) ensure "${2:-}" ;;
    base) base "${2:?usage: toolchain.sh base <libc> [arch]}" "${3:-$(uname -m)}" ;;
    platform) platform "${2:?usage: toolchain.sh platform <libc> [arch]}" "${3:-$(uname -m)}" ;;
    *) echo "usage: toolchain.sh files|key|image <libc>|ensure <libc>|base <libc> [arch]|platform <libc> [arch]" >&2
        exit 2 ;;
esac
