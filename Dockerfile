# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Builds the selkies Python wheel, with the HTML5 web client built from source
# inside this same repo (addons/) and bundled into the wheel. No external or
# pre-published images are consumed.

# 1) Build the web client (core + dashboard + keyboard-layout DB + touch gamepad)
FROM docker.io/library/node:26-alpine AS web-build

ARG SELKIES_MODE=webrtc
ARG SELKIES_UPLOAD_DIR=/home/ubuntu/Desktop

WORKDIR /build

COPY addons/selkies-web-core ./selkies-web-core
COPY addons/selkies-dashboard ./selkies-dashboard
COPY addons/universal-touch-gamepad ./universal-touch-gamepad
# Web PWA icon/favicon are vendored in this repository, not downloaded. The
# plated icon is the installed app's; the browser tab keeps the bare mark the
# dashboard ships as icon.png
COPY docs/assets /repo-assets

RUN set -eux; \
    cd selkies-web-core; \
    npm install --no-audit --no-fund; \
    npm run build; \
    cd ../selkies-dashboard; \
    cp ../selkies-web-core/dist/selkies-core.js src/; \
    npm install --no-audit --no-fund; \
    SELKIES_INJECT=1 SELKIES_MODE="${SELKIES_MODE}" SELKIES_UPLOAD_DIR="${SELKIES_UPLOAD_DIR}" npm run build; \
    mkdir -p dist/src; \
    cp ../selkies-web-core/dist/selkies-core.js dist/src/; \
    cp ../universal-touch-gamepad/universalTouchGamepad.js dist/src/; \
    cp -r ../selkies-web-core/dist/jsdb dist/; \
    mkdir -p /webout; \
    cp -ar dist/. /webout/; \
    printf '%s' '{"name":"Selkies","short_name":"Selkies","manifest_version":2,"version":"1.0.0","display":"fullscreen","background_color":"#000000","theme_color":"#000000","icons":[{"src":"icon-512.png","type":"image/png","sizes":"512x512"}],"start_url":"/"}' > /webout/manifest.json; \
    cp /repo-assets/logo/icon-512x512.png /webout/icon-512.png; \
    cp /repo-assets/logo/favicon.ico /webout/favicon.ico

# 2) Build the Python wheel with the web client bundled
FROM docker.io/library/python:3-slim AS py-build

LABEL maintainer="https://github.com/danisla,https://github.com/ehfd"

ARG PYPI_PACKAGE=selkies
ARG PACKAGE_VERSION=0.0.0.dev0

RUN python3 -m pip install --no-cache-dir --upgrade build

WORKDIR /opt/pypi

COPY src ./src
COPY README.md pyproject.toml ./
# Include the production built web files in the wheel package
COPY --from=web-build /webout ./src/selkies/selkies_web

# Patch the package name and version
RUN sed -i \
    -e "s|^name =.*|name = \"${PYPI_PACKAGE}\"|g" \
    -e "s|^version =.*|version = \"${PACKAGE_VERSION}\"|g" \
    pyproject.toml

RUN python3 -m build
