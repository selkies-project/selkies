#!/bin/bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

SCRIPT_DIR=$(readlink -f $(dirname $0))

# Change this to set the Linux distribution and version
DISTRIB_IMAGE=ubuntu
DISTRIB_RELEASE=20.04

(
    cd ${SCRIPT_DIR?}/.. && \
    GSTREAMER_BASE_IMAGE=gstreamer TEST_IMAGE=selkies-example:latest-${DISTRIB_IMAGE}${DISTRIB_RELEASE} DISTRIB_IMAGE=${DISTRIB_IMAGE} DISTRIB_RELEASE=${DISTRIB_RELEASE} docker-compose run --service-ports test
)