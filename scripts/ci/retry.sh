#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Run a command, retrying it after a pause. For the registry operations whose
# rate-limit response reaches the caller as a failed command rather than as a
# Retry-After header it could read; scripts/ci/fetch.sh covers the HTTP
# downloads where that header is available.
#
# Usage: retry.sh <attempts> <delay-seconds> <command> [args...]

set -eu

ATTEMPTS="${1:?usage: retry.sh <attempts> <delay-seconds> <command> [args...]}"
DELAY="${2:?usage: retry.sh <attempts> <delay-seconds> <command> [args...]}"
shift 2

attempt=1
until "$@"; do
    if [ "${attempt}" -ge "${ATTEMPTS}" ]; then
        echo "retry: $1 failed after ${attempt} attempts" >&2
        exit 1
    fi
    echo "retry: $1 failed, waiting ${DELAY}s (attempt ${attempt}/${ATTEMPTS})" >&2
    sleep "${DELAY}"
    attempt="$((attempt + 1))"
done
