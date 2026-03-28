#!/bin/sh
set -e -x

# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

INSTALL_DIR=${INSTALL_DIR:-"/usr/share/nginx/html"}
mkdir -p "${INSTALL_DIR}"

echo "INFO: Installing to ${INSTALL_DIR}"

###
# Install sources
###
mkdir -p "${INSTALL_DIR}/lib"
cp src/lib/*.js "${INSTALL_DIR}/lib/"

mkdir -p "${INSTALL_DIR}/css"
cp -R src/css/* "${INSTALL_DIR}/css/"

cp src/*.js src/*.html src/manifest.json src/*.png src/*.ico "${INSTALL_DIR}/"

###
# Patch index.html to fetch latest version of javascript source
# Patch the service worker with a new cache version so that it is refreshed.
###
(cd "${INSTALL_DIR}" && \
    export CACHE_VERSION=$(date +%s) && \
    sed -i 's|script src="\(.*\)?ts=.*"|script src="\1?ts='${CACHE_VERSION}'"|g' index.html && \
    sed -i "s|CACHE_VERSION|${CACHE_VERSION}|g" sw.js app.js)

echo "INFO: Done."