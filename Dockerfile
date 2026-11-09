# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Builds the selkies wheel, with the HTML5 web client built from source
# inside this same repo (web/) and compiled into its server. No external or
# pre-published images are consumed. The wheel is for the platform of
# BUILD_IMAGE, a manylinux2014, manylinux_2_28, or musllinux_1_2 image of this
# machine's architecture, and lands in /opt/build/dist.
ARG BUILD_IMAGE="quay.io/pypa/manylinux2014_x86_64"

# 1) Build the web client through the same script the CI wheel build and every
#    other channel run, so every channel ships an identical bundle
FROM node:26-alpine AS web-build

WORKDIR /build

# build-web.sh reads these paths relative to the repository root and writes
# the bundle to dist/web
COPY Cargo.toml ./
COPY scripts/ci/build-web.sh ./scripts/ci/
COPY web/selkies-web-core ./web/selkies-web-core
COPY web/selkies-dashboard ./web/selkies-dashboard
COPY web/universal-touch-gamepad ./web/universal-touch-gamepad
COPY docs/assets/logo ./docs/assets/logo

# The controller database's GitHub lookups use the build secret github_token
# where one is given (`--secret id=github_token,env=GITHUB_TOKEN`)
RUN --mount=type=secret,id=github_token sh scripts/ci/build-web.sh

# 2) Build the wheel, its server compiling the web client in
FROM ${BUILD_IMAGE} AS py-build

LABEL maintainer="https://github.com/danisla,https://github.com/ehfd"

ARG PACKAGE_VERSION="0.0.0.dev0"

WORKDIR /opt/build

COPY . ./
# The production build of the web client, which the server embeds
COPY --from=web-build /build/dist/web ./dist/web

# Stamps the version and builds the server with the codecs pixelflux's own
# wheels build
RUN SELKIES_VERSION="${PACKAGE_VERSION}" infra/wheel/build.sh /opt/build/dist
