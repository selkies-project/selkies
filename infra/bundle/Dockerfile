# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# The bundle's toolchain image for one C library and architecture (TARGET:
# glibc-x86_64, glibc-aarch64, musl-x86_64, or musl-aarch64): the builder image
# whose C library is the bundle's floor, manylinux_2_28 or musllinux_1_2, pinned
# here by digest, with everything a bundle carries or links that this tree does
# not compile itself built into it under SELKIES_TOOLCHAIN: the tools and headers
# deps.sh installs, the Rust toolchain (infra/packaging/rust.sh), the X servers
# and their static dependencies (xservers.sh), CUPS (cups.sh), the codecs
# (infra/packaging/codecs.sh), libxkbcommon (infra/packaging/xkbcommon.sh),
# PipeWire's headers (pipewire.sh), dbus-daemon (dbus.sh), and, on glibc,
# NVIDIA's X11 EGL platform (egl-x11.sh). infra/bundle/build-in-image.sh, run in
# it, then compiles only the server, the shell, and the interposers, and finds
# each of these where it left it, once the key the image records matches its
# tree's; run in a bare builder image, or in one built from other files, it
# builds them first.
#
# Each component builds in a stage of its own from the tools, so the stages run
# beside one another and a change to one script rebuilds that stage alone. The
# image is named for these files (infra/bundle/toolchain.sh) and built from
# them alone: its context is the list `toolchain.sh files` prints, never the
# tree.
ARG TARGET
FROM quay.io/pypa/manylinux_2_28_x86_64:2026.11.13-1@sha256:15b2f7023f20045b764d4d2afbcfae92a198ef6fb8706c6fa3f7c9adc08d0c5f AS glibc-x86_64
FROM quay.io/pypa/manylinux_2_28_aarch64:2026.11.13-1@sha256:42248b7127c8a2fefd5d9ccb7ee80e70b7708c889208e86c78514f04d1279e69 AS glibc-aarch64
FROM quay.io/pypa/musllinux_1_2_x86_64:2026.11.13-1@sha256:bb0ac84fb133550d3479fdcc1dd55d4f4f32558dbc80a173d6fdd01fed2e9eff AS musl-x86_64
FROM quay.io/pypa/musllinux_1_2_aarch64:2026.11.13-1@sha256:42e7c99e0bc64418741f04747f68accac9d83b345141dfd1eee81201c1e8d5f9 AS musl-aarch64

FROM ${TARGET} AS tools
ENV SELKIES_TOOLCHAIN="/opt/selkies-toolchain" RUSTUP_HOME="/opt/rust/rustup" CARGO_HOME="/opt/rust/cargo"
COPY infra/packaging/portable-deps.sh /toolchain/infra/packaging/
COPY crates/pcmflux/scripts/build-libpulse.sh /toolchain/crates/pcmflux/scripts/
COPY infra/bundle/deps.sh /toolchain/infra/bundle/
RUN sh /toolchain/infra/bundle/deps.sh && touch /etc/selkies-bundle-deps
COPY infra/packaging/rust.sh /toolchain/infra/packaging/
RUN sh /toolchain/infra/packaging/rust.sh

FROM tools AS codecs
COPY crates/pixelflux/pyproject.toml /toolchain/crates/pixelflux/
COPY crates/pixelflux/patches /toolchain/crates/pixelflux/patches
COPY infra/packaging/codecs.sh /toolchain/infra/packaging/
RUN PATH="/opt/python/cp312-cp312/bin:${PATH}" \
        sh /toolchain/infra/packaging/codecs.sh "${SELKIES_TOOLCHAIN}/codecs" /toolchain/crates/pixelflux \
    && rm -rf "${SELKIES_TOOLCHAIN}/codecs/src"

# The libraries the interposers and the server build against without linking
# anything of the build image's: libxkbcommon, PipeWire's headers, and NVIDIA's
# X11 EGL platform, which only glibc's bundle carries; and the session's bus
FROM tools AS libraries
COPY infra/packaging/xkbcommon.sh /toolchain/infra/packaging/
RUN PATH="/opt/python/cp312-cp312/bin:${PATH}" BUILD=/tmp sh -c '. /toolchain/infra/packaging/xkbcommon.sh'
COPY infra/bundle/pipewire.sh infra/bundle/egl-x11.sh infra/bundle/dbus.sh /toolchain/infra/bundle/
RUN sh /toolchain/infra/bundle/dbus.sh "${SELKIES_TOOLCHAIN}/dbus" \
    && sh /toolchain/infra/bundle/pipewire.sh "${SELKIES_TOOLCHAIN}/pipewire" \
    && if ! ldd --version 2>&1 | grep -qi musl; then \
        sh /toolchain/infra/bundle/egl-x11.sh "${SELKIES_TOOLCHAIN}/egl-x11"; \
    fi

# The X servers, then CUPS, which links their static zlib
FROM tools AS xservers
COPY infra/bundle/xservers.sh infra/bundle/gbm-loader.c infra/bundle/gl-loader.c /toolchain/infra/bundle/
COPY infra/bundle/patches /toolchain/infra/bundle/patches
COPY addons/base/patches /toolchain/addons/base/patches
RUN mkdir -p /tmp/tree && sh /toolchain/infra/bundle/xservers.sh /tmp/tree \
    && cd "${SELKIES_TOOLCHAIN}/xservers" && rm -rf /tmp/tree src sources glvnd xkb dri-pc
COPY infra/bundle/cups.sh /toolchain/infra/bundle/
RUN mkdir -p /tmp/tree && sh /toolchain/infra/bundle/cups.sh /tmp/tree \
    && rm -rf /tmp/tree "${SELKIES_TOOLCHAIN}/cups/sources"

FROM xservers
LABEL org.opencontainers.image.source="https://github.com/selkies-project/selkies"
LABEL org.opencontainers.image.description="The toolchain the Selkies bundle is built in, for one C library and architecture"
COPY --from=codecs /opt/selkies-toolchain/ /opt/selkies-toolchain/
COPY --from=libraries /opt/selkies-toolchain/ /opt/selkies-toolchain/
# The key of the files it was built from, which a bundle build compares with its
# tree's before it takes anything from here
COPY . /toolchain/
RUN sh /toolchain/infra/bundle/toolchain.sh key > "${SELKIES_TOOLCHAIN}/key"
