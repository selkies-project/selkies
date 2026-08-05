#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# Download a URL, waiting as long as the server asks when it rate-limits.
#
# curl retries 429 and 5xx by itself and honours their Retry-After, but GitHub
# reports a secondary rate limit as 403, which curl treats as a hard failure.
# This loop reads Retry-After off that response and sleeps for it, falling back
# to a widening delay when the header is absent.
#
# Usage: fetch.sh <url> <output-path>

set -eu

URL="${1:?usage: fetch.sh <url> <output-path>}"
OUT="${2:?usage: fetch.sh <url> <output-path>}"
ATTEMPTS="${FETCH_ATTEMPTS:-5}"
BACKOFF="${FETCH_BACKOFF:-30}"

attempt=1
while :; do
    headers="$(mktemp)"
    if curl -fsSL --retry 3 --retry-max-time 120 -D "${headers}" -o "${OUT}" "${URL}"; then
        rm -f "${headers}"
        exit 0
    fi
    # The last status line and Retry-After win: -L means redirects add their own
    status="$(awk 'tolower($0) ~ /^http\// { s = $2 } END { print s }' "${headers}")"
    delay="$(awk 'tolower($0) ~ /^retry-after:/ { gsub(/\r/, "", $2); d = $2 } END { print d }' "${headers}")"
    rm -f "${headers}"

    if [ "${attempt}" -ge "${ATTEMPTS}" ]; then
        echo "fetch: giving up on ${URL} after ${attempt} attempts (HTTP ${status:-?})" >&2
        exit 1
    fi
    case "${delay}" in
        '' | *[!0-9]*) delay="$((BACKOFF * attempt))" ;;
    esac
    echo "fetch: HTTP ${status:-?} for ${URL}, waiting ${delay}s (attempt ${attempt}/${ATTEMPTS})" >&2
    sleep "${delay}"
    attempt="$((attempt + 1))"
done
