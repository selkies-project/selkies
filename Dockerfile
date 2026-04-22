# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

FROM ghcr.io/selkies-project/selkies/selkies-web:main as selkies-web
FROM python:3

LABEL maintainer="https://github.com/danisla,https://github.com/ehfd"

# Install build deps
ARG PIP_BREAK_SYSTEM_PACKAGES=1
RUN python3 -m pip install --no-cache-dir --force-reinstall --upgrade build

# Build a python package for the webrtc app.
WORKDIR /opt/pypi

# Copy source files
COPY src ./src
COPY README.md pyproject.toml ./
# Include the production built web files in the wheel package
COPY --from=selkies-web /usr/share/nginx/html ./src/selkies/selkies_web

ARG PYPI_PACKAGE=selkies
ARG PACKAGE_VERSION=0.0.0.dev0

# Patch the package name and version
RUN sed -i \
    -e "s|^name =.*|name = \"${PYPI_PACKAGE}\"|g" \
    -e "s|^version =.*|version = \"${PACKAGE_VERSION}\"|g" \
    pyproject.toml

RUN python3 -m build
