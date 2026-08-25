# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Builds the selkies Python wheel, with the HTML5 web client built from source
# inside this same repo (addons/) and bundled into the wheel. No external or
# pre-published images are consumed.

# 1) Build the web client through the same script the CI wheel build and the
#    conda recipe run, so every channel ships an identical bundle
FROM docker.io/library/node:26-alpine AS web-build

WORKDIR /build

# build-web.sh reads these paths relative to the repository root and writes
# the bundle to src/selkies/selkies_web
COPY pyproject.toml ./
COPY scripts/ci/build-web.sh ./scripts/ci/
COPY addons/selkies-web-core ./addons/selkies-web-core
COPY addons/selkies-dashboard ./addons/selkies-dashboard
COPY addons/selkies-dashboard-wish ./addons/selkies-dashboard-wish
COPY addons/universal-touch-gamepad ./addons/universal-touch-gamepad
COPY docs/assets/logo ./docs/assets/logo

RUN mkdir -p src/selkies && sh scripts/ci/build-web.sh

# 2) Build the Python wheel with the web client bundled
FROM docker.io/library/python:3-slim AS py-build

LABEL maintainer="https://github.com/danisla,https://github.com/ehfd"

ARG PYPI_PACKAGE="selkies"
ARG PACKAGE_VERSION="0.0.0.dev0"

# Set through the environment rather than as flags on the install below,
# because the install is not the only pip this stage runs: `python3 -m build`
# provisions an isolated environment of its own and pip-installs the build
# backend into it from PyPI, and that call is reached through the environment
# or not at all.
ENV PIP_RETRIES="5" \
    PIP_TIMEOUT="60"

RUN python3 -m pip install --no-cache-dir --upgrade build

WORKDIR /opt/pypi

COPY src ./src
COPY README.md pyproject.toml ./
# Include the production built web files in the wheel package
COPY --from=web-build /build/src/selkies/selkies_web ./src/selkies/selkies_web

# Patch the package name and version
RUN sed -i \
    -e "s|^name =.*|name = \"${PYPI_PACKAGE}\"|g" \
    -e "s|^version =.*|version = \"${PACKAGE_VERSION}\"|g" \
    pyproject.toml

RUN python3 -m build
