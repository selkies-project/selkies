#!/bin/sh

# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# This file incorporates work covered by the following copyright and
# permission notice:
#
#   Copyright 2019 Google LLC
#
#   Licensed under the Apache License, Version 2.0 (the "License");
#   you may not use this file except in compliance with the License.
#   You may obtain a copy of the License at
#
#        http://www.apache.org/licenses/LICENSE-2.0
#
#   Unless required by applicable law or agreed to in writing, software
#   distributed under the License is distributed on an "AS IS" BASIS,
#   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
#   See the License for the specific language governing permissions and
#   limitations under the License.

set -e

export TURN_EXTERNAL_IP="${TURN_EXTERNAL_IP:-$(detect_external_ip)}"

# NOTE that the listening IP must be bound to only the IPs you will be responding to if not using "0.0.0.0" or "::".
# Binding to the wrong IP(s) can result in connectivity issues that are difficult to trace.
# Typically $(hostname -i) will return the primary IP to listen on.

# shellcheck disable=SC2086  # TURN_EXTRA_ARGS carries whole flags, split on purpose
turnserver \
    --verbose \
    --listening-ip="0.0.0.0" \
    --listening-ip="::" \
    --listening-port="${TURN_PORT:-3478}" \
    --aux-server="0.0.0.0:${TURN_ALT_PORT:-8443}" \
    --aux-server="[::]:${TURN_ALT_PORT:-8443}" \
    --realm="${TURN_REALM:-example.com}" \
    --external-ip="${TURN_EXTERNAL_IP}" \
    --min-port="${TURN_MIN_PORT:-49152}" \
    --max-port="${TURN_MAX_PORT:-65535}" \
    --channel-lifetime="${TURN_CHANNEL_LIFETIME:--1}" \
    --use-auth-secret \
    --static-auth-secret="${TURN_SHARED_SECRET:-changeme}" \
    --no-cli \
    --cli-password="$(tr -dc 'A-Za-z0-9' < /dev/urandom 2>/dev/null | head -c 24)" \
    --userdb="/tmp/turnserver-turndb" \
    --pidfile="/tmp/turnserver.pid" \
    --log-file="stdout" \
    --allow-loopback-peers \
    --prometheus \
    ${TURN_EXTRA_ARGS} "$@"
