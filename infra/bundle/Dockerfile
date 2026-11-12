# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# The image the bundle is built in: manylinux_2_28, the bundle's glibc floor,
# with what infra/bundle/deps.sh installs, kept as a layer so a rebuild of the
# bundle compiles only the tree. Built by infra/bundle/build.sh from the
# scripts copied here; it changes only when one of them does.
ARG MANYLINUX=quay.io/pypa/manylinux_2_28_x86_64
FROM ${MANYLINUX}
COPY infra/packaging/portable-deps.sh /deps/infra/packaging/portable-deps.sh
COPY crates/pcmflux/scripts/build-libpulse.sh /deps/crates/pcmflux/scripts/build-libpulse.sh
COPY infra/bundle/deps.sh /deps/infra/bundle/deps.sh
RUN sh /deps/infra/bundle/deps.sh && touch /etc/selkies-bundle-deps
