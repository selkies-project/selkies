# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

FROM python:3

LABEL maintainer "https://github.com/danisla,https://github.com/ehfd"

# Install build deps
RUN python3 -m pip install --no-cache-dir --force-reinstall --upgrade build

# Build a python package for the webrtc app.
WORKDIR /opt/pypi

# Copy source files
COPY src ./src
COPY pyproject.toml setup.cfg ./

ARG PYPI_PACKAGE=selkies-gstreamer
ARG PACKAGE_VERSION=1.0.0

# Patch the package name and version
RUN sed -i \
    -e "s|^name =.*|name = ${PYPI_PACKAGE}|g" \
    -e "s|^version =.*|version = ${PACKAGE_VERSION}|g" \
    setup.cfg

RUN python3 -m build
